# Spec O, part 2: Daemon mode and the agent sidecar — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `balerix serve --mode kubernetes` that serves TLS and takes resolved fleets from an operator, and a `balerix-agent` binary whose `sidecar` materialises one agent in a pod, drives it over a tmux socket shared with the `agent` container, forwards Claude's hooks, and holds one outbound WebSocket link to the Daemon over which `send_text`, `send_keys`, `stop`, `restart`, `attach` and the workspace reads arrive. Done when the two-process integration tests under `agent/tests/` pass (Spec O §17.2).

**Architecture:** The sidecar is a one-agent daemon: it runs the existing `balerix-core` planner over `balerix-runtime`'s `Runtime` (materializer) and `TmuxRunner` (runner), both given a pod layout (`StateLayout::pod`) and a socket *path* instead of a socket name. In pod mode the runner's process waits read no `/proc`: a stop respawns the pane into a waiter that outlives the supervisor and polls tmux's own `pane_dead`. The Daemon in Kubernetes mode runs no planner for a pod fleet: its actor mirrors the sidecar's `status` frames, forwards the stopped set as `stop`/`restart` frames, and answers plugin calls (`send_text`, `attach`, workspace reads) through a `LinkHub` that implements `AgentRunner` and `WorkspaceReader` over the link. TLS is `axum-server` with `rustls` on the ring provider, serving from a mounted certificate; the sidecar trusts one authority file. Plugins are not in this sub-project: a Kubernetes-mode Daemon reads no `plugins.yaml`, launches no plugin, and `PUT /v1/plugins` arrives with §9 (sub-project 4).

**Tech Stack:** Rust 1.99.0 (edition 2024, `unsafe` forbidden), tokio 1.53.1, axum 0.8.9 (`ws`), axum-server 0.8.0 (`tls-rustls-no-provider`), rustls 0.23.45 (`ring`), rustls-pki-types 1.15.1, tokio-tungstenite 0.30.0, reqwest 0.13.4, rcgen 0.14.10 (tests only), clap 4.6.6, tmux 3.7c, nono 0.79.0, mise, git 2.47.3.

**Spec:** `docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md` (§6, §7, §12, §15, §19; §4.4 and §5.4 for what the operator will mount; §8.1 for the shared slice). The sidecar's stop/restart mechanism and the Daemon's mirror actor are decided here (see **Decisions this plan makes**) and are written into the spec as §7.4 in Task 14.

## Global Constraints

- Branch `feat/kube-daemon-sidecar`, cut from `main` at `e19bcc0` or later. One pull request, squash-merged, title `feat(server): Kubernetes mode and the balerix-agent sidecar over a TLS link (Spec O §6, §7)`.
- The uncommitted `mise.toml` change (`claude = "2.1.287"` → `"2.1.288"`) is this branch's first commit (Task 1). AGENTS.md: a `claude` bump needs `mise run verify-claude` and `mise run verify-questions` by hand before merge (Task 14).
- Run cargo only through mise: `mise x -- cargo …` or `mise run <task>`. From outside `/workspace` mise finds no rust; stay in the repository root.
- `mise run check` (core workspace) passes before every commit; `mise run agent` (the new standalone project) passes before every commit from Task 10 on; `mise run test-it` passes before the commits of Tasks 4 and 5.
- No `unsafe` anywhere (the workspace forbids it; the `agent/` project forbids it too). `unwrap`/`expect` are warnings outside tests; `#![allow(clippy::unwrap_used, clippy::expect_used)]` at the top of every test file, as every existing test file does.
- Exact versions for every new dependency, with a comment saying why, in the project that uses it: core dependencies in the root `[workspace.dependencies]`, `agent/` dependencies in `agent/Cargo.toml`.
- `scripts/check-core-deps.sh` must keep passing unchanged: the core workspace stays the seven crates, and the core `reqwest` keeps exactly the `json` feature. Nothing in this plan enables a `reqwest` feature in the core workspace; `axum-server` and `rustls` are separate dependencies. (§10.3's change to that script belongs to the sub-project that makes the Daemon a TLS *client*, §9.)
- Spec O §6.1: the pod runs as uid 10001 with a read-only root filesystem; everything the sidecar writes goes under the agent claim (`/balerix/agent`) or the run directory (`/balerix/run`). Nothing is written under `/tmp` or `$HOME` by the sidecar.
- Spec O §6.3: every tmux client call in pod mode passes `-u`; `pane_pid` is opaque in the sidecar (reported, never resolved under `/proc`); pod-mode waits use `pane_dead` and never `/proc`.
- Spec O §7.1: the hook route the sidecar serves is `POST /v1/agents/{fleet}/{crew}/{agent}/events` on `127.0.0.1`, authenticated with `Authorization: Bearer <agent token>`, exactly the Daemon's route today; a Daemon that cannot be reached answers `200 {}`.
- Spec O §10.3: tokens and keys are mounted files. No secret in argv, env or logs; types holding a token or a key hand-implement `Debug` and print `<redacted>`.
- Test roots under `target/tmp` (`CARGO_TARGET_TMPDIR`), never `/tmp`: nono grants `/tmp` by default, and the pod-mode tests assert on what the sandbox refuses.
- `BALERIX_REQUIRE_TOOLS=1` turns every "skip: tool missing" into a failure; CI sets it for the `check` and the new `agent` jobs.
- Error text that tests pin, verbatim: `agent processes still running after stop (pid <n>)` (unchanged), `SandboxUnavailable: <nono's first stderr line>`, `fleet <name> is managed by kubernetes; change it through its Fleet object`, `agent_tokens is accepted only by a daemon in kubernetes mode`, `agent_tokens: no token for <fleet>/<crew>/<agent>`, `<fleet>/<crew>/<agent>: link down` (the new `RunnerError::Link { id, message }`), `agent_tokens: use PUT /v1/fleets/{name}`.

## Review Focus

Conditions the spec implies and no task's tests would otherwise exercise, most likely to bite first. Each has a test in the task that owns it.

1. **A locale-less sidecar.** Without `-u` tmux turns the tab-separated window format into `agent_0_56_` and `observe` fails to parse (§19.1). Task 5 runs the pod-mode runner with `LANG`, `LC_ALL` and `LC_CTYPE` removed from the test's environment and asserts `observe` parses.
2. **A pid reused in the agent container.** The pod-mode stop waits on `pane_dead`, which the waiter sets when `kill -0 <pid>` fails; a reused pid would hold the waiter until the bound. Task 5 asserts the bound is the existing 5 s `StillRunning`, with the pid in the message, and that the window is not killed before the pane is dead (the spec's rule).
3. **The crew cache mounted objects-only and read-only.** `git clone --reference` must accept a directory holding only `.git/objects` (probed on git 2.47.3: it does, and writes that path into `objects/info/alternates`), and nothing in `materialize` may write into it. Task 4 materialises against a 0555 objects-only cache and asserts the clone works and the cache's mtime set is unchanged.
4. **The Daemon unreachable while Claude posts a hook.** The sidecar must answer `200 {}` inside Claude's hook budget and count the failure, never block. Task 11 points the forwarder at a closed port and at a server that sleeps past the budget, and asserts both answer `{}` within the bound.
5. **A `stop` that arrives while the link is down.** The Daemon records the stopped set; the sidecar must learn it when it reconnects. Task 7 reconciles the stopped set against every incoming `status` frame (a frame saying `Ready` for an agent in the set gets a `stop`; one saying `Stopped` for an agent not in the set gets a `restart`) and pins it with a fake sidecar.

## Decisions this plan makes

The spec leaves these to sub-project 2 (memory: "mechanism for stop/restart is sub-project 2's to design"). They are recorded in the spec as §7.4 by Task 14.

- **The Daemon mirrors; it does not plan.** For a fleet with `owner: kubernetes` the actor runs no `reconcile_pass`. `status` frames from sidecars are its only observed state (written into `FleetStatus.agents` as sent); `SetStopped` becomes a `stop` or `restart` frame; `Down` sends `stop` to every linked agent, clears the agents and marks the fleet `Down` (the pods are the operator's to delete). Two planners over one agent would double every restart.
- **A pod-mode stop is a respawn into a waiter.** From the sidecar nothing can signal the pane's process, and `/proc` is another container's. `respawn-window -k` delivers the hangup to the supervisor as `kill-window` does today, but into a replacement command that runs *in the agent container*: `/bin/sh -c 'while kill -0 <pane_pid> 2>/dev/null; do sleep 0.02; done'`. The supervisor exits only when its tree is empty; the waiter exits when the supervisor is gone; tmux marks the pane dead; the sidecar polls `pane_dead` with the existing 5 s bound, then kills the window so the agent is *absent*, which is what leaving the stopped set expects. The restart arm of `ensure_agent` does the same without the final `kill-window`.
- **Readiness is a file.** Agent pods accept no inbound connections (O-10), so the readiness probe is `exec: test -f /balerix/run/ready`. The sidecar writes the marker on `SessionStart` and removes it when the phase leaves `Ready`.
- **The operator's apply is `PUT /v1/fleets/{name}` with `agent_tokens`.** `FleetRequest` gains an optional `agent_tokens` map (`fleet/crew/agent` → token). A Kubernetes-mode Daemon takes a body carrying it as the operator's, records the fleet with `owner: kubernetes` and the tokens as the agents' hook secrets (so the hook route and the link authenticate with the one token the operator minted). A body without it on such a fleet, and `POST`/`DELETE` without `force`, answer 409, as for a plugin-owned fleet. In tmux mode a body with `agent_tokens` is a 400.
- **Kubernetes mode is flags on `serve`.** `--mode kubernetes --tls-cert <pem> --tls-key <pem> --admin-token-file <file>`; `--bind` may then be a non-loopback address. The Daemon reads no `plugins.yaml` and launches no plugin in this mode; its system-pool is `Ready` without installing anything (the shared volume's daemon pool is a Job's, §8.3). `GET /readyz` answers 200 when the pool channel says `Ready`, else 503 with the reason.
- **The `agent` container learns its session from the start marker.** `balerix-agent run` waits for `/balerix/run/started`, whose content is the crew's session name (`<fleet>/<crew>`), then starts the tmux server with the crew's anchor window and polls `has-session` until the server is gone. It needs no Secret and no arguments.
- **Mount paths are flags with the spec's defaults.** `--agent-dir /balerix/agent`, `--shared-dir /balerix/shared`, `--run-dir /balerix/run`, `--bundle /balerix/secret/agent.json`, `--ca /balerix/tls/ca.crt`, `--termination-log /dev/termination-log`. Tests point them at temp roots. Under `--shared-dir`: `repo/.git/objects` (the crew cache's objects, read-only), `crew/mise`, `fleet/mise`, `daemon/mise` (the three pools, read-only). Sub-project 3 mounts the shared claim's sub-paths there.
- **File bytes over the link are a JSON array of numbers.** `workspace.file` is capped at 1 MiB by `WORKSPACE_FILE_LIMIT`; a base64 encoding would add a dependency to the leaf `balerix-api` crate for a quarter of the frame size. tungstenite's 64 MiB message limit is nowhere near.

---

## File Structure

| File | Change |
|---|---|
| `mise.toml` | the `claude` pin (already edited); tasks `agent`, `fmt`, `audit` entries |
| `crates/balerix-api/src/link.rs` | **new**: the link frames (`LinkRequest`, `LinkOp`, `LinkReply`, `LinkResult`, `LinkFailure`, `FailureKind`, `LinkStatus`, `SidecarFrame`, `LINK_PROTOCOL`) |
| `crates/balerix-api/src/bundle.rs` | **new**: `AgentBundle`, what the operator mounts for a sidecar |
| `crates/balerix-api/src/request.rs` | `FleetRequest.agent_tokens` |
| `crates/balerix-api/src/lib.rs` | the two modules and their re-exports |
| `crates/balerix-runtime/src/layout.rs` | `PodMounts`, `PodLayout`, `StateLayout::pod`, `pod_layout()`; `crew`, `fleet`, `agent`, `mise_data_dir` consult it |
| `crates/balerix-runtime/src/materializer.rs` | pod mode: `ensure_crew` checks the slice; `materialize` unchanged in shape |
| `crates/balerix-runtime/src/workspace.rs` | `create_clone` skips the cache fetch when the layout is a pod |
| `crates/balerix-runtime/src/sandbox.rs` | `sandbox_self_test`, `SelfTestError` |
| `crates/balerix-runtime/src/tmux.rs` | `TmuxRunner::at_socket`, `socket_path`, `-u`, `wait_pane_dead`, the waiter respawn |
| `crates/balerix-runtime/tests/support/mod.rs` | `layout()` through a constructor; `pod_layout()` |
| `crates/balerix-runtime/tests/{layout_pod_it,materialize_pod_it,tmux_pod_it}.rs` | **new** integration tests |
| `crates/balerix-server/src/kube/mod.rs` | **new**: `pub use`; `KUBERNETES_OWNER` |
| `crates/balerix-core/src/ports.rs` | `RunnerError::Link { id, message }` |
| `crates/balerix-server/src/kube/link.rs` | **new**: `LinkHub`, `LinkError`, the two link routes' handlers, `impl AgentRunner`, `impl WorkspaceReader` |
| `crates/balerix-server/src/kube/pty.rs` | **new**: `WsPty`, the attach pump |
| `crates/balerix-server/src/kube/idle.rs` | **new**: `NoFiles` (`Materializer`), `NoPool` (`SystemToolchain`) |
| `crates/balerix-server/src/kube/tls.rs` | **new**: `serve_tls`, `TlsServer` |
| `crates/balerix-server/src/actor.rs` | `Ports.kube`, `Msg::Apply.agent_tokens`, `Msg::LinkStatus`, `Msg::LinkDown`, the mirror pass |
| `crates/balerix-server/src/daemon.rs` | `Caller::Kubernetes`, `check_owner`, `apply_kube`, `link_status`, `link_down`, `kube()`, `system_pool_state()` |
| `crates/balerix-server/src/api.rs` | `agent_tokens` dispatch in `update_fleet`/`create_fleet`, `/readyz`, the two link routes |
| `crates/balerix-server/src/testing.rs` | `Harness::kube()` |
| `crates/balerix-server/tests/{kube_link_it,kube_api_it,kube_attach_it}.rs` | **new** |
| `crates/balerix/src/cli.rs`, `commands/serve.rs` | `--mode`, `--tls-cert`, `--tls-key`, `--admin-token-file`; the Kubernetes wiring |
| `crates/balerix/tests/cli_serve.rs` | the TLS test |
| `agent/` | **new** standalone project `balerix-agent`: `Cargo.toml`, `Cargo.lock`, `clippy.toml`, `deny.toml`, `src/{lib,main,cli,bundle,tls,hooks,link,attach,sidecar,run}.rs`, `tests/{cli_it,run_it,hooks_it,link_it,sidecar_it}.rs`, `tests/support/mod.rs` |
| `scripts/agent.sh` | **new**: fmt/check for `agent/`, building `balerix` first for the tests |
| `.github/workflows/ci.yml` | the `agent` job |
| `docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md` | §7.4 (the decisions above), a §12 line for `kube/tls.rs` |
| `ARCHITECTURE.md`, `AGENTS.md`, `docs/THREAT-MODEL.md` | the pieces, the task, the boundary and the rows |

---

### Task 1: The branch and the `claude` pin

**Files:**
- Modify: `mise.toml:14` (already edited in the working tree: `claude = "2.1.288"`)

**Interfaces:**
- Produces: branch `feat/kube-daemon-sidecar`; the embedded tool table (`balerix-runtime/src/toolchain.rs` `include_str!("../../../mise.toml")`) now hands agents claude 2.1.288.

- [ ] **Step 1: Cut the branch over the dirty tree**

```bash
cd /workspace
git status --short            # expect exactly: ` M mise.toml`
git checkout -b feat/kube-daemon-sidecar
git diff mise.toml
```
Expected diff: one line, `-claude = "2.1.287"` / `+claude = "2.1.288"`.

- [ ] **Step 2: Install the pinned version and confirm the gate**

```bash
mise install claude
mise x -- claude --version
mise run check
```
Expected: `2.1.288 (Claude Code)` (or the version string that 2.1.288 prints; it must contain `2.1.288`); `check` passes. No committed snapshot embeds the pin (`grep -rn "2\.1\.287" --include=*.snap crates` prints nothing), so nothing else changes.

- [ ] **Step 3: Commit**

```bash
git add mise.toml
git commit -m "chore(tools): claude 2.1.288

The embedded tool table hands agents this version. verify-claude and
verify-questions run on this branch before merge (AGENTS.md)."
```

---

### Task 2: `balerix-api`: the link frames, the operator's request, the agent bundle

**Files:**
- Create: `crates/balerix-api/src/link.rs`
- Create: `crates/balerix-api/src/bundle.rs`
- Modify: `crates/balerix-api/src/request.rs:8-14`
- Modify: `crates/balerix-api/src/lib.rs:10-21, 32-37`

**Interfaces:**
- Produces, for Tasks 6–8 and 11–13:
  - `balerix_api::link::{LINK_PROTOCOL, LinkRequest, LinkOp, LinkReply, LinkResult, LinkFailure, FailureKind, LinkStatus, SidecarFrame}` (exact shapes below)
  - `balerix_api::AgentBundle` with fields `agent: String`, `repo: String`, `git_ref: String`, `git: GitSettings`, `settings: AgentSettings`, `daemon_url: String`, `token: String`, `credentials: CredentialBundle`
  - `balerix_api::FleetRequest.agent_tokens: Option<BTreeMap<String, String>>`
- Wire shapes (JSON): a Daemon → sidecar text frame is a `LinkRequest`, `{"id":7,"op":{"kind":"send_text","text":"hi","submit":true}}`; a sidecar → Daemon text frame is a `SidecarFrame`, `{"frame":"reply","id":7,"result":{"kind":"ok"}}` or `{"frame":"status","status":{…AgentStatus…},"pid":56,"hook_failures":0}`.

- [ ] **Step 1: Write the failing serde tests for the link frames**

Create `crates/balerix-api/src/link.rs` with the tests first (the module body comes in Step 3):

```rust
//! The sidecar link (Spec O §7.2): JSON text frames on one WebSocket from
//! a pod's sidecar to its Daemon. The Daemon sends requests, one per id;
//! the sidecar answers each with a reply carrying the same id, and sends a
//! `status` frame on every change of its agent's status. Terminal bytes
//! for `attach` never travel here: an `attach` request names a session,
//! and the sidecar opens a second socket for it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{AgentStatus, KeyStep, WorkspaceDiff, WorkspaceTree, WorkspaceVersion};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentPhase;
    use serde_json::json;

    #[test]
    fn requests_are_tagged_by_kind_under_op() {
        let r = LinkRequest {
            id: 7,
            op: LinkOp::SendText {
                text: "hi".into(),
                submit: true,
            },
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({ "id": 7, "op": { "kind": "send_text", "text": "hi", "submit": true } })
        );
        let back: LinkRequest = serde_json::from_value(json!({
            "id": 8, "op": { "kind": "workspace_file", "path": "src/main.rs" }
        }))
        .unwrap();
        assert_eq!(
            back.op,
            LinkOp::WorkspaceFile {
                path: "src/main.rs".into()
            }
        );
        let stop: LinkRequest =
            serde_json::from_value(json!({ "id": 1, "op": { "kind": "stop" } })).unwrap();
        assert_eq!(stop.op, LinkOp::Stop);
        assert!(
            serde_json::from_value::<LinkRequest>(json!({ "id": 1, "op": { "kind": "fly" } }))
                .is_err()
        );
    }

    #[test]
    fn replies_and_status_are_tagged_by_frame() {
        let reply = SidecarFrame::Reply(LinkReply {
            id: 7,
            result: LinkResult::Ok,
        });
        assert_eq!(
            serde_json::to_value(&reply).unwrap(),
            json!({ "frame": "reply", "id": 7, "result": { "kind": "ok" } })
        );
        let failed = SidecarFrame::Reply(LinkReply {
            id: 9,
            result: LinkResult::Failed {
                failure: LinkFailure {
                    reason: FailureKind::TooLarge { limit: 1048576 },
                    message: "file is larger than 1 MiB".into(),
                },
            },
        });
        let v = serde_json::to_value(&failed).unwrap();
        assert_eq!(v["result"]["kind"], "failed");
        assert_eq!(v["result"]["failure"]["reason"], "too_large");
        assert_eq!(v["result"]["failure"]["limit"], 1048576);
        assert_eq!(v["result"]["failure"]["message"], "file is larger than 1 MiB");
        let back: SidecarFrame = serde_json::from_value(v).unwrap();
        assert_eq!(back, failed);
        let status = SidecarFrame::Status(LinkStatus {
            status: AgentStatus {
                phase: AgentPhase::Ready,
                ..AgentStatus::default()
            },
            pid: Some(56),
            hook_failures: 2,
        });
        let v = serde_json::to_value(&status).unwrap();
        assert_eq!(v["frame"], "status");
        assert_eq!(v["status"]["phase"], "ready");
        assert_eq!(v["pid"], 56);
        let back: SidecarFrame = serde_json::from_value(v).unwrap();
        assert!(matches!(back, SidecarFrame::Status(s) if s.pid == Some(56) && s.hook_failures == 2));
    }

    #[test]
    fn file_bytes_travel_as_a_json_array() {
        let r = LinkResult::File {
            bytes: vec![0, 255, 10],
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({ "kind": "file", "bytes": [0, 255, 10] })
        );
    }

    #[test]
    fn the_protocol_is_one() {
        assert_eq!(LINK_PROTOCOL, 1);
    }
}
```

- [ ] **Step 2: Run them to see the module fail to compile**

Run: `mise x -- cargo test -p balerix-api link::`
Expected: compile errors (`LinkRequest` and friends undefined).

- [ ] **Step 3: Write the types**

Insert between the `use` lines and `#[cfg(test)]` in `crates/balerix-api/src/link.rs`:

```rust
/// Bumped when a frame changes shape. The sidecar sends it as the
/// `balerix-link-protocol` header on connect; the Daemon refuses another
/// value with 400.
pub const LINK_PROTOCOL: u32 = 1;

/// The header carrying `LINK_PROTOCOL`.
pub const LINK_PROTOCOL_HEADER: &str = "balerix-link-protocol";

/// Daemon → sidecar: one request. The reply carries the same `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkRequest {
    pub id: u64,
    pub op: LinkOp,
}

/// What a Daemon asks of a sidecar (§7.2's table). `SendText`, `SendKeys`
/// and `Attach` are `AgentRunner`'s; `Stop` and `Restart` move the
/// sidecar's stopped set (plugins spec §16.4); the four `Workspace*` are
/// `WorkspaceReader`'s, run by the sidecar in the agent's clone.
///
/// `deny_unknown_fields` holds for the struct variants; for the unit ones
/// serde ignores extra keys (serde-rs/serde#1358, the reason `PluginAction`
/// hand-writes its `Deserialize`). Harmless here: an extra key on `stop`
/// changes nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LinkOp {
    SendText { text: String, submit: bool },
    SendKeys { steps: Vec<KeyStep>, delay_ms: u64 },
    Stop,
    Restart,
    /// The sidecar opens `GET /v1/agents/{id}/link/attach/{session}` and
    /// answers `Ok` once that socket is up.
    Attach { session: String },
    WorkspaceDiff { base_ref: String },
    WorkspaceFile { path: String },
    WorkspaceTree { path: String },
    WorkspaceVersion { base_ref: String },
}

/// Sidecar → Daemon: the answer to one request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkReply {
    pub id: u64,
    pub result: LinkResult,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LinkResult {
    Ok,
    Diff { diff: WorkspaceDiff },
    /// The file's bytes as a JSON array; at most `WORKSPACE_FILE_LIMIT`.
    File { bytes: Vec<u8> },
    Tree { tree: WorkspaceTree },
    Version { version: WorkspaceVersion },
    /// A struct variant, not a newtype: the payload carries its own tag
    /// (`reason`), and an internally tagged newtype variant would merge
    /// the two objects into one.
    Failed { failure: LinkFailure },
}

/// Why a request failed, in the sidecar's words. `reason` lets the Daemon
/// rebuild the port error (`WorkspaceError` or `RunnerError`) the plugin
/// route maps to a status; `message` is the sidecar's error text. No
/// `deny_unknown_fields`: serde does not combine it with `flatten`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkFailure {
    #[serde(flatten)]
    pub reason: FailureKind,
    pub message: String,
}

/// `WorkspaceError`'s variants, plus `Runner` for a tmux failure. Tagged
/// `reason` so it can be flattened next to `message`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum FailureKind {
    Missing,
    NoSuchPath,
    InvalidPath,
    NotAFile,
    NotADirectory,
    TooLarge { limit: u64 },
    Tool,
    Filter,
    Io,
    Runner,
}

/// Sidecar → Daemon on every change: the agent's status as the sidecar's
/// own planner keeps it, the pane's pid as tmux reports it (opaque to the
/// Daemon, §6.3), and how many hooks the sidecar answered for a Daemon it
/// could not reach (§7.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkStatus {
    pub status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default)]
    pub hook_failures: u64,
}

/// Every text frame a sidecar sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum SidecarFrame {
    Status(LinkStatus),
    Reply(LinkReply),
}

/// `fleet/crew/agent` → token, the operator's `agent_tokens` map.
pub type AgentTokens = BTreeMap<String, String>;
```

The wire shape of a failure is `{"kind":"failed","failure":{"reason":"too_large","limit":1048576,"message":"…"}}`; the Step 1 test is the contract.

- [ ] **Step 4: Run the link tests**

Run: `mise x -- cargo test -p balerix-api link::`
Expected: 4 passed.

- [ ] **Step 5: Write the failing tests for `AgentBundle` and `agent_tokens`**

Create `crates/balerix-api/src/bundle.rs`:

```rust
//! What the operator mounts for one sidecar (Spec O §5.4): the resolved
//! agent, the credential bundle and the Daemon token, as one JSON file.
//! Never printed: `Debug` redacts the token and the credentials.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{AgentSettings, CredentialBundle, GitSettings};

/// `/balerix/secret/agent.json`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentBundle {
    /// `fleet/crew/agent`.
    pub agent: String,
    /// The crew's repository, as the fleet file wrote it.
    pub repo: String,
    /// The crew's base ref.
    pub git_ref: String,
    #[serde(default)]
    pub git: GitSettings,
    pub settings: AgentSettings,
    /// `https://<daemon service>:<port>`.
    pub daemon_url: String,
    /// The agent's token: its hook secret and its link credential.
    pub token: String,
    #[serde(default)]
    pub credentials: CredentialBundle,
}

impl fmt::Debug for AgentBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentBundle")
            .field("agent", &self.agent)
            .field("repo", &self.repo)
            .field("git_ref", &self.git_ref)
            .field("daemon_url", &self.daemon_url)
            .field("token", &"<redacted>")
            .field("credentials", &self.credentials)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bundle() -> AgentBundle {
        serde_json::from_value(json!({
            "agent": "payments/backend/alice",
            "repo": "acme/payments-api",
            "git_ref": "main",
            "settings": { "claude": { "settings": { "model": "sonnet" } } },
            "daemon_url": "https://balerix-default.team-a.svc:7643",
            "token": "tok-SECRET-0123456789abcdef0123456789abcdef",
            "credentials": { "gh_token": "gho_SECRET" }
        }))
        .unwrap()
    }

    #[test]
    fn debug_redacts_the_token_and_the_credentials() {
        let dbg = format!("{:?}", bundle());
        assert!(!dbg.contains("SECRET"), "{dbg}");
        assert!(dbg.contains("payments/backend/alice"));
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn round_trips_and_refuses_unknown_fields() {
        let b = bundle();
        let back: AgentBundle = serde_json::from_str(&serde_json::to_string(&b).unwrap()).unwrap();
        assert_eq!(back, b);
        assert!(
            serde_json::from_value::<AgentBundle>(json!({ "agent": "f/c/a", "x": 1 })).is_err()
        );
    }
}
```

Add to the tests in `crates/balerix-api/src/request.rs`:

```rust
    #[test]
    fn agent_tokens_is_optional_and_absent_on_the_wire_by_default() {
        let r = FleetRequest {
            spec: FleetSpec {
                name: "f".into(),
                crews: BTreeMap::new(),
                ..Default::default()
            },
            credentials: CredentialBundle::default(),
            agent_tokens: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("agent_tokens").is_none());
        let with: FleetRequest = serde_json::from_value(serde_json::json!({
            "spec": { "name": "f" },
            "agent_tokens": { "f/c/a": "t1" }
        }))
        .unwrap();
        assert_eq!(with.agent_tokens.unwrap()["f/c/a"], "t1");
    }
```

- [ ] **Step 6: Run them to see them fail**

Run: `mise x -- cargo test -p balerix-api`
Expected: compile errors (`bundle` module missing, `agent_tokens` field unknown).

- [ ] **Step 7: Wire the modules and the field**

In `crates/balerix-api/src/request.rs`, change `FleetRequest`:

```rust
/// Body of `POST /v1/fleets` (up) and `PUT /v1/fleets/{name}` (update).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetRequest {
    pub spec: FleetSpec,
    #[serde(default)]
    pub credentials: CredentialBundle,
    /// Spec O §7.3: the operator's `PUT` carries one token per agent
    /// (`fleet/crew/agent` → token); the fleet is then recorded with
    /// `owner: kubernetes`. Refused with 400 by a daemon not in
    /// Kubernetes mode. Absent from every CLI request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_tokens: Option<std::collections::BTreeMap<String, String>>,
}
```

Add `agent_tokens: None,` to the existing `request_round_trips` test's literal. In `crates/balerix-api/src/lib.rs` add `pub mod bundle;` and `pub mod link;` to the module list (alphabetical) and these re-exports:

```rust
pub use bundle::AgentBundle;
pub use link::{
    AgentTokens, FailureKind, LINK_PROTOCOL, LINK_PROTOCOL_HEADER, LinkFailure, LinkOp, LinkReply,
    LinkRequest, LinkResult, LinkStatus, SidecarFrame,
};
```

Every other constructor of `FleetRequest` in the repository needs `agent_tokens: None`: run `grep -rn "FleetRequest {" crates/ plugins/` and add the field to each literal (the CLI's `commands/fleet.rs`, the server's tests, the SDK's). Build everything: `mise x -- cargo build --workspace --all-targets`.

- [ ] **Step 8: Run the gate and commit**

Run: `mise run check`
Expected: passes (the api crate packages cleanly; `deny_unknown_fields` on `FleetRequest` still holds for the CLI's bodies).

```bash
git add crates/balerix-api crates/balerix/src crates/balerix-server crates/balerix-plugin-sdk
git commit -m "feat(api): the sidecar link frames, agent_tokens on PUT /v1/fleets, the agent bundle (Spec O §7)"
```

---

### Task 3: `balerix-runtime`: the pod layout

**Files:**
- Modify: `crates/balerix-runtime/src/layout.rs:8-12, 97-180`
- Modify: `crates/balerix-runtime/src/lib.rs:24`
- Modify: `crates/balerix-runtime/tests/support/mod.rs:36-42`
- Create: `crates/balerix-runtime/tests/layout_pod_it.rs`

**Interfaces:**
- Consumes: `StateLayout { state_root, data_root, config_root }` and its path methods (Task-independent, existing).
- Produces, for Tasks 4, 5, 13:
  - `pub struct PodMounts { pub agent: PathBuf, pub shared: PathBuf, pub run: PathBuf }`
  - `pub struct PodLayout { pub mounts: PodMounts, pub id: AgentId }` with `tmux_socket() -> PathBuf` (`<run>/tmux.sock`), `start_marker() -> PathBuf` (`<run>/started`), `ready_marker() -> PathBuf` (`<run>/ready`), `shared_repo() -> PathBuf` (`<shared>/repo`)
  - `StateLayout::pod(mounts: PodMounts, id: &AgentId) -> StateLayout`; `StateLayout::xdg(state_root, data_root, config_root) -> StateLayout` (what the struct literal was); `StateLayout::pod_layout(&self) -> Option<&PodLayout>`
  - In a pod layout: `agent(id).root == <agent>`, `crew(id.crew_ref()) == CrewPaths { root: <shared>/crew, repo: <shared>/repo }`, `fleet(f) == FleetPaths { root: <shared>/fleet, mise_toml: <shared>/fleet/mise.toml }`, `mise_data_dir() == <shared>/daemon/mise`, so `agent_pools(id) == [<shared>/crew/mise, <shared>/fleet/mise, <shared>/daemon/mise]` (crew, fleet, daemon: §8.1's order). `state_root`, `data_root`, `config_root` are `<agent>/.balerix/{state,data,config}`.

- [ ] **Step 1: Write the failing integration test**

Create `crates/balerix-runtime/tests/layout_pod_it.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §6.1 and §8.1: every path still comes from `StateLayout`, and in
//! a pod they land on the three mounts.

use std::path::PathBuf;

use balerix_core::AgentId;
use balerix_runtime::layout::{PodMounts, StateLayout};

fn mounts() -> PodMounts {
    PodMounts {
        agent: PathBuf::from("/balerix/agent"),
        shared: PathBuf::from("/balerix/shared"),
        run: PathBuf::from("/balerix/run"),
    }
}

#[test]
fn the_agent_lives_at_the_claim_root_and_the_crew_slice_on_the_shared_mount() {
    let id: AgentId = "payments/backend/alice".parse().unwrap();
    let layout = StateLayout::pod(mounts(), &id);
    let a = layout.agent(&id);
    assert_eq!(a.root, PathBuf::from("/balerix/agent"));
    assert_eq!(a.home, PathBuf::from("/balerix/agent/home"));
    assert_eq!(a.workspace, PathBuf::from("/balerix/agent/workspace"));
    assert_eq!(a.nono_home, PathBuf::from("/balerix/agent/nono"));
    assert_eq!(a.launch, PathBuf::from("/balerix/agent/launch.sh"));
    assert_eq!(a.logs, PathBuf::from("/balerix/agent/logs"));
    assert_eq!(a.profile, PathBuf::from("/balerix/agent/nono-profile.json"));
    assert_eq!(a.installed_marker(), PathBuf::from("/balerix/agent/.installed"));
    let c = layout.crew(&id.crew_ref());
    assert_eq!(c.repo, PathBuf::from("/balerix/shared/repo"));
    assert_eq!(
        c.cache_objects(),
        PathBuf::from("/balerix/shared/repo/.git/objects")
    );
    assert_eq!(c.mise_pool(), PathBuf::from("/balerix/shared/crew/mise"));
    assert_eq!(
        layout.fleet(&id.fleet).mise_pool(),
        PathBuf::from("/balerix/shared/fleet/mise")
    );
    assert_eq!(
        layout.mise_data_dir(),
        PathBuf::from("/balerix/shared/daemon/mise")
    );
    assert_eq!(
        layout.shared_install_dirs(&id),
        "/balerix/shared/crew/mise/installs:/balerix/shared/fleet/mise/installs:/balerix/shared/daemon/mise/installs"
    );
    let pod = layout.pod_layout().unwrap();
    assert_eq!(pod.tmux_socket(), PathBuf::from("/balerix/run/tmux.sock"));
    assert_eq!(pod.start_marker(), PathBuf::from("/balerix/run/started"));
    assert_eq!(pod.ready_marker(), PathBuf::from("/balerix/run/ready"));
    // whatever else asks for a root lands on the claim, never on the
    // read-only root filesystem
    assert_eq!(
        layout.server_dir(),
        PathBuf::from("/balerix/agent/.balerix/state/server")
    );
    assert_eq!(
        layout.system_mise_toml(),
        PathBuf::from("/balerix/agent/.balerix/config/mise.toml")
    );
}

#[test]
fn another_agent_of_the_same_pod_layout_is_still_under_the_state_root() {
    // The layout is for one agent; a sibling's paths exist (the planner
    // may name them) but are ordinary state-root paths, never the claim.
    let id: AgentId = "payments/backend/alice".parse().unwrap();
    let other: AgentId = "payments/backend/bob".parse().unwrap();
    let layout = StateLayout::pod(mounts(), &id);
    assert_eq!(
        layout.agent(&other).root,
        PathBuf::from("/balerix/agent/.balerix/state/fleets/payments/crews/backend/agents/bob")
    );
}

#[test]
fn an_xdg_layout_has_no_pod() {
    let layout = StateLayout::xdg(
        PathBuf::from("/s"),
        PathBuf::from("/d"),
        PathBuf::from("/c"),
    );
    assert!(layout.pod_layout().is_none());
    let id: AgentId = "f/c/a".parse().unwrap();
    assert_eq!(
        layout.agent(&id).root,
        PathBuf::from("/s/fleets/f/crews/c/agents/a")
    );
    assert_eq!(layout.mise_data_dir(), PathBuf::from("/d/mise"));
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `mise x -- cargo test -p balerix-runtime --test layout_pod_it`
Expected: compile errors (`PodMounts`, `pod`, `xdg`, `pod_layout` undefined).

- [ ] **Step 3: Add the pod layout**

In `crates/balerix-runtime/src/layout.rs`, replace the struct at lines 8-12 with:

```rust
/// The three roots everything hangs off. On one machine they are the XDG
/// roots (`from_env`); in a pod (`pod`) they sit under the agent claim and
/// the one agent's and its crew's paths come from the mounts instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateLayout {
    pub state_root: PathBuf,
    pub data_root: PathBuf,
    pub config_root: PathBuf,
    pod: Option<PodLayout>,
}

/// The three mounts of an agent pod (Spec O §6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodMounts {
    /// The agent claim: `home/`, `workspace/`, `nono/`, `logs/`, the
    /// profiles, `launch.sh`.
    pub agent: PathBuf,
    /// Read-only sub-paths of the Daemon's shared claim: `repo/.git/objects`
    /// (the crew cache's objects) and `crew/mise`, `fleet/mise`,
    /// `daemon/mise` (the three pools, §8.1).
    pub shared: PathBuf,
    /// An `emptyDir` both containers mount: the tmux socket and the markers.
    pub run: PathBuf,
}

/// A pod layout: the mounts and the one agent they are for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodLayout {
    pub mounts: PodMounts,
    pub id: AgentId,
}

impl PodLayout {
    /// The tmux server's socket, shared by the two containers (§6.2).
    pub fn tmux_socket(&self) -> PathBuf {
        self.mounts.run.join("tmux.sock")
    }
    /// Written by the sidecar once the agent is materialised and the
    /// sandbox self-test passed; its content is the crew's tmux session
    /// name. The `agent` container waits for it (§6.2 step 3).
    pub fn start_marker(&self) -> PathBuf {
        self.mounts.run.join("started")
    }
    /// Present while the agent is `Ready`; the pod's exec readiness probe.
    pub fn ready_marker(&self) -> PathBuf {
        self.mounts.run.join("ready")
    }
    /// The crew cache as the pod sees it: only `.git/objects` is mounted.
    pub fn shared_repo(&self) -> PathBuf {
        self.mounts.shared.join("repo")
    }
}
```

Replace `from_env`'s `Self { … }` literal with `Self::xdg(pick(…), pick(…), pick(…))` and add, right after `from_env`:

```rust
    /// The three roots given directly (tests, `dev materialize`).
    pub fn xdg(state_root: PathBuf, data_root: PathBuf, config_root: PathBuf) -> Self {
        Self {
            state_root,
            data_root,
            config_root,
            pod: None,
        }
    }

    /// Spec O §6.1: the one agent at the claim root, its crew's cache and
    /// the three pools on the shared mount, and anything else that asks
    /// for a root under `<agent>/.balerix/`, which is on the claim and so
    /// writable in a pod whose root filesystem is not.
    pub fn pod(mounts: PodMounts, id: &AgentId) -> Self {
        let roots = mounts.agent.join(".balerix");
        Self {
            state_root: roots.join("state"),
            data_root: roots.join("data"),
            config_root: roots.join("config"),
            pod: Some(PodLayout {
                mounts,
                id: id.clone(),
            }),
        }
    }

    pub fn pod_layout(&self) -> Option<&PodLayout> {
        self.pod.as_ref()
    }
```

Change `mise_data_dir`, `crew`, `fleet` and `agent` to consult the pod:

```rust
    pub fn mise_data_dir(&self) -> PathBuf {
        match &self.pod {
            Some(p) => p.mounts.shared.join("daemon").join("mise"),
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
                repo: p.shared_repo(),
                root: p.mounts.shared.join("crew"),
            };
        }
        let root = self.fleet_dir(&c.fleet).join("crews").join(c.crew.as_str());
        CrewPaths {
            repo: root.join("repo"),
            root,
        }
    }
    pub fn fleet(&self, f: &FleetName) -> FleetPaths {
        if let Some(p) = &self.pod
            && p.id.fleet == *f
        {
            let root = p.mounts.shared.join("fleet");
            return FleetPaths {
                mise_toml: root.join("mise.toml"),
                root,
            };
        }
        let root = self.fleet_dir(f);
        FleetPaths {
            mise_toml: root.join("mise.toml"),
            root,
        }
    }
    pub fn agent(&self, id: &AgentId) -> AgentPaths {
        let root = match &self.pod {
            Some(p) if p.id == *id => p.mounts.agent.clone(),
            _ => self
                .crew(&id.crew_ref())
                .root
                .join("agents")
                .join(id.agent.as_str()),
        };
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
```

Watch the sibling case in `agent`: for `bob` in a pod layout for `alice`, `self.crew(&id.crew_ref())` returns the *shared* crew paths (same crew), so `bob`'s root would be `<shared>/crew/agents/bob`. The second test pins the state-root form instead; make the `_` arm compute the crew root without the pod: `self.fleet_dir(&id.fleet).join("crews").join(id.crew.as_str()).join("agents").join(id.agent.as_str())`.

`CrewRef` needs `PartialEq` (it derives it) and `AgentId::crew_ref()` exists. Export the new types from `crates/balerix-runtime/src/lib.rs`: `pub use layout::{AgentPaths, CrewPaths, FleetPaths, PluginPaths, PodLayout, PodMounts, StateLayout, shared_list};`.

- [ ] **Step 4: Replace every struct literal with the constructor**

The private `pod` field breaks the five literals. Change each to `StateLayout::xdg(…)`:

- `crates/balerix-runtime/tests/support/mod.rs:36-42`: `pub fn layout(root: &Path) -> StateLayout { StateLayout::xdg(root.join("state"), root.join("data"), root.join("config")) }`, and add
  ```rust
  /// A pod layout on temp mounts: `<root>/agent`, `<root>/shared`, `<root>/run`.
  pub fn pod_layout(root: &Path, id: &balerix_core::AgentId) -> StateLayout {
      StateLayout::pod(
          balerix_runtime::layout::PodMounts {
              agent: root.join("agent"),
              shared: root.join("shared"),
              run: root.join("run"),
          },
          id,
      )
  }
  ```
- `crates/balerix-runtime/tests/generated_golden.rs:52`, `crates/balerix-runtime/tests/plugin_golden.rs:14`, `crates/balerix-runtime/src/materializer.rs:434`, `crates/balerix/src/commands/dev.rs:62`: the same three-argument `xdg` call with the literal's three values, in the order `state_root, data_root, config_root`.

- [ ] **Step 5: Run the tests**

Run: `mise x -- cargo test -p balerix-runtime --test layout_pod_it` then `mise x -- cargo test -p balerix-runtime` and `mise x -- cargo test -p balerix`
Expected: the three new tests pass; the golden snapshots are unchanged (`xdg` produces the same paths the literals did); nothing else changes.

- [ ] **Step 6: Gate and commit**

Run: `mise run check`

```bash
git add crates/balerix-runtime crates/balerix/src/commands/dev.rs
git commit -m "feat(runtime): the pod layout, with the agent at the claim root and the crew slice on the shared mount (Spec O §6.1)"
```

---

### Task 4: `balerix-runtime`: materialising against the read-only crew slice

**Files:**
- Modify: `crates/balerix-runtime/src/materializer.rs:310-331` (`ensure_crew`)
- Modify: `crates/balerix-runtime/src/workspace.rs:759-790` (`create_clone`)
- Modify: `crates/balerix-runtime/src/sandbox.rs` (append `sandbox_self_test`)
- Modify: `crates/balerix-runtime/src/lib.rs:31-34` (exports)
- Create: `crates/balerix-runtime/tests/materialize_pod_it.rs`

**Interfaces:**
- Consumes: `StateLayout::pod`, `support::pod_layout` (Task 3); `Runtime::new(layout, tools)`, `Materializer::{ensure_crew, materialize}` (existing); `validate_profile`'s way of running nono (`sandbox.rs:372-410`: `nono -s profile validate <profile>` with `HOME=<nono_home>`).
- Produces, for Task 13:
  - In a pod layout, `Runtime::ensure_crew` writes nothing: it checks `crew.cache_objects()` is a directory and returns `MaterializeError::Invalid { id: "<fleet>/<crew>", message: "crew cache not synced: <path> is missing (the crew sync Job has not run)" }` otherwise.
  - In a pod layout, `create_clone` does not run `git -C <cache> fetch`; the clone still carries `--reference <cache>`.
  - `pub fn sandbox_self_test(tools: &ToolPaths, paths: &AgentPaths) -> Result<(), SelfTestError>` with
    ```rust
    #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
    pub enum SelfTestError {
        /// nono exited 1 with `Landlock not available` (Spec O §19.2).
        #[error("SandboxUnavailable: {0}")]
        Unavailable(String),
        /// Any other non-zero exit; the first stderr line.
        #[error("sandbox self-test failed: {0}")]
        Failed(String),
        #[error("sandbox self-test could not run nono: {0}")]
        Io(String),
    }
    ```

- [ ] **Step 1: Write the failing integration tests**

Create `crates/balerix-runtime/tests/materialize_pod_it.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §6.2 step 1 in a pod layout: the sidecar materialises an agent
//! over a crew cache it can only read, mounted objects-only.
mod support;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use balerix_api::{AgentSettings, CredentialBundle, GitAuth, GitSettings};
use balerix_core::{AgentId, HookTarget, MaterializeError, Materializer, RepoRef, ResolvedAgent};
use balerix_runtime::sandbox::{SelfTestError, sandbox_self_test};
use balerix_runtime::{Runtime, StateLayout};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// An origin with one commit, and a crew cache of it holding only
/// `.git/objects`, read-only, the way the pod mounts it.
fn origin_and_objects_only_cache(root: &Path, layout: &StateLayout, id: &AgentId) -> RepoRef {
    let origin = root.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main"]);
    std::fs::write(origin.join("f"), "one\n").unwrap();
    git(&origin, &["add", "f"]);
    git(&origin, &["commit", "-q", "-m", "one"]);
    let full = root.join("cache-full");
    let out = Command::new("git")
        .args(["clone", "-q", "--bare"])
        .arg(&origin)
        .arg(&full)
        .output()
        .unwrap();
    assert!(out.status.success());
    let objects = layout.crew(&id.crew_ref()).cache_objects();
    std::fs::create_dir_all(objects.parent().unwrap()).unwrap();
    let cp = Command::new("cp")
        .arg("-r")
        .arg(full.join("objects"))
        .arg(&objects)
        .output()
        .unwrap();
    assert!(cp.status.success());
    chmod_tree(&objects, 0o555, 0o444);
    RepoRef::parse(&format!("file://{}", origin.display())).unwrap()
}

fn chmod_tree(dir: &Path, dirs: u32, files: u32) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            chmod_tree(&p, dirs, files);
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(dirs)).unwrap();
        } else {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(files)).unwrap();
        }
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(dirs)).unwrap();
}

fn mtimes(dir: &Path) -> BTreeMap<String, std::time::SystemTime> {
    let mut out = BTreeMap::new();
    fn walk(dir: &Path, out: &mut BTreeMap<String, std::time::SystemTime>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            out.insert(p.display().to_string(), e.metadata().unwrap().modified().unwrap());
            if p.is_dir() {
                walk(&p, out);
            }
        }
    }
    walk(dir, &mut out);
    out
}

fn agent(id: &AgentId, repo: RepoRef) -> ResolvedAgent {
    ResolvedAgent {
        id: id.clone(),
        repo,
        git_ref: "main".into(),
        git: GitSettings {
            push: false,
            auth: GitAuth::None,
            ..GitSettings::default()
        },
        settings: AgentSettings::default(),
    }
}

#[test]
fn a_pod_materialize_clones_from_the_objects_only_cache_and_writes_nothing_into_it() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("materialize-pod");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let id: AgentId = "f/c/a".parse().unwrap();
    let layout = support::pod_layout(&root, &id);
    let repo = origin_and_objects_only_cache(&root, &layout, &id);
    let objects = layout.crew(&id.crew_ref()).cache_objects();
    let before = mtimes(&objects);
    let rt = Runtime::new(layout.clone(), tools.clone());
    let creds = CredentialBundle::default();
    let fleet_tools = BTreeMap::new();
    let crew_tools = BTreeMap::new();
    rt.ensure_crew(
        &id.crew_ref(),
        &repo,
        "main",
        &GitSettings {
            push: false,
            auth: GitAuth::None,
            ..GitSettings::default()
        },
        &creds,
        balerix_core::CrewTools {
            fleet: &fleet_tools,
            crew: &crew_tools,
        },
    )
    .unwrap();
    assert!(
        !layout.crew(&id.crew_ref()).mise_pool().exists(),
        "a pod ensure_crew installs no pool: the Job did"
    );
    let plan = rt
        .materialize(
            &agent(&id, repo),
            &creds,
            &HookTarget {
                url: "http://127.0.0.1:7643".into(),
                secret: "s".into(),
            },
        )
        .unwrap();
    let a = layout.agent(&id);
    assert_eq!(plan.script, a.launch);
    assert!(a.workspace.join(".git").is_dir());
    let alternates =
        std::fs::read_to_string(a.workspace.join(".git/objects/info/alternates")).unwrap();
    assert_eq!(alternates.trim(), objects.display().to_string());
    assert_eq!(mtimes(&objects), before, "the cache was written");
    assert!(a.installed_marker().is_file());
    chmod_tree(&objects, 0o755, 0o644); // let TempRoot remove it
}

#[test]
fn a_pod_ensure_crew_without_a_synced_cache_names_the_job() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("materialize-pod-nocache");
    let id: AgentId = "f/c/a".parse().unwrap();
    let layout = support::pod_layout(&root, &id);
    let rt = Runtime::new(layout.clone(), tools);
    let empty = BTreeMap::new();
    let err = rt
        .ensure_crew(
            &id.crew_ref(),
            &RepoRef::parse("acme/api").unwrap(),
            "main",
            &GitSettings::default(),
            &CredentialBundle::default(),
            balerix_core::CrewTools {
                fleet: &empty,
                crew: &empty,
            },
        )
        .unwrap_err();
    let objects = layout.crew(&id.crew_ref()).cache_objects();
    assert_eq!(
        err,
        MaterializeError::Invalid {
            id: "f/c".into(),
            message: format!(
                "crew cache not synced: {} is missing (the crew sync Job has not run)",
                objects.display()
            ),
        }
    );
}

#[test]
fn the_sandbox_self_test_passes_where_landlock_works_and_names_an_unavailable_sandbox() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("selftest");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let id: AgentId = "f/c/a".parse().unwrap();
    let layout = support::pod_layout(&root, &id);
    let repo = origin_and_objects_only_cache(&root, &layout, &id);
    let rt = Runtime::new(layout.clone(), tools.clone());
    let out = rt
        .render_agent(
            &agent(&id, repo),
            &CredentialBundle::default(),
            &HookTarget {
                url: "http://127.0.0.1:7643".into(),
                secret: "s".into(),
            },
            &balerix_runtime::RenderOptions::default(),
        )
        .unwrap();
    assert!(out.plan.script.is_file());
    let paths = layout.agent(&id);
    sandbox_self_test(&tools, &paths).unwrap();

    // A nono that cannot set the sandbox up: the message nono gave at the
    // spike (§19.2), exit 1. The sidecar's termination message starts with
    // `SandboxUnavailable:`.
    let fake = root.join("fake-nono");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho 'nono: Sandbox initialization failed: Landlock not available. Requires Linux kernel 5.13+ with Landlock enabled.' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut broken = tools.clone();
    broken.nono = fake;
    assert_eq!(
        sandbox_self_test(&broken, &paths),
        Err(SelfTestError::Unavailable(
            "nono: Sandbox initialization failed: Landlock not available. Requires Linux kernel 5.13+ with Landlock enabled.".into()
        ))
    );
    assert_eq!(
        sandbox_self_test(&broken, &paths).unwrap_err().to_string(),
        "SandboxUnavailable: nono: Sandbox initialization failed: Landlock not available. Requires Linux kernel 5.13+ with Landlock enabled."
    );
    let other = root.join("other-nono");
    std::fs::write(&other, "#!/bin/sh\necho 'nono: profile: bad' >&2\nexit 2\n").unwrap();
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o755)).unwrap();
    broken.nono = other;
    assert_eq!(
        sandbox_self_test(&broken, &paths),
        Err(SelfTestError::Failed("nono: profile: bad".into()))
    );
    let objects = layout.crew(&id.crew_ref()).cache_objects();
    chmod_tree(&objects, 0o755, 0o644);
}
```

`GitSettings` has more fields than `push` and `auth` (`fleet.rs:46-`); the `..GitSettings::default()` covers them. `RenderOptions` and `render_agent` are public (`materializer.rs:27, 47`).

- [ ] **Step 2: Run to see them fail**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo test -p balerix-runtime --test materialize_pod_it`
Expected: compile error on `sandbox_self_test`; after stubbing, the first test fails in `ensure_crew` (it tries to write the pool and `ensure_repo` into the read-only slice) and the second returns a different error.

- [ ] **Step 3: Pod mode in `ensure_crew` and `create_clone`**

In `crates/balerix-runtime/src/materializer.rs`, at the top of `impl Materializer for Runtime`'s `ensure_crew`, before the gh block:

```rust
        let id = crew.to_string();
        // Spec O §8.3: in a pod the cache and the pools are a Job's work,
        // mounted read-only; the sidecar only checks the cache is there.
        if self.layout.pod_layout().is_some() {
            let objects = self.layout.crew(crew).cache_objects();
            if !objects.is_dir() {
                return Err(MaterializeError::Invalid {
                    id,
                    message: format!(
                        "crew cache not synced: {} is missing (the crew sync Job has not run)",
                        objects.display()
                    ),
                });
            }
            return Ok(());
        }
```
(and drop the now-duplicate `let id = crew.to_string();` below it).

In `crates/balerix-runtime/src/workspace.rs`, `create_clone` needs to know the layout; it only has `Workspace { tools, gh_config_dir }`. Add a field to `Workspace`:

```rust
pub struct Workspace<'a> {
    pub tools: &'a ToolPaths,
    /// `GH_CONFIG_DIR` for the daemon's git calls when `git.auth: gh`.
    pub gh_config_dir: Option<PathBuf>,
    /// Spec O §8.1: the cache is a read-only mount kept current by a Job;
    /// a new clone skips the fetch into it.
    pub cache_is_read_only: bool,
}
```

Set `cache_is_read_only: self.layout.pod_layout().is_some()` in `Runtime::workspace` (`materializer.rs:255-260`) and `cache_is_read_only: false` in the two `Workspace { … }` literals in `remove_agent` and `remove_crew`, and in every other literal (`grep -rn "Workspace {" crates/balerix-runtime`). In `create_clone`, wrap the first `self.git(…fetch…)` call:

```rust
        if !self.cache_is_read_only {
            self.git(
                id,
                crew,
                &["-C", &cache, "fetch", "--quiet", "--no-auto-gc", "origin"],
            )?;
        }
```

`check_clone` (filesystem checks on the clone and the cache's `alternates`) is unchanged: reading the mount is fine. The later `git_probe … rev-parse` on the cache reads the objects-only directory; `rev-parse --verify refs/heads/<branch>` in a directory with no refs answers "not a repository"? Check: `git -C <objects-only-parent> rev-parse` fails with exit 128 ("not a git repository") rather than the quiet "no such ref" exit 1 that `git_probe` expects. Make the probe conditional as well: with `cache_is_read_only`, skip the harvested-branch seed (`cached = None`), because a pod's cache slice holds objects only and no harvested branch can be seen through it (the Job's harvest writes refs into the cache the operator reads; seeding from them is sub-project 3's to wire if it wants it). Read `create_clone` after the clone (`workspace.rs:790-860`) and gate the `rev-parse`/`merge-base` probes and the seed fetch on `!self.cache_is_read_only`; the plain `checkout -B <branch> origin/<start_ref>` path runs.

- [ ] **Step 4: The sandbox self-test**

Append to `crates/balerix-runtime/src/sandbox.rs`:

```rust
/// Spec O §6.2 step 2: `nono -s run --profile <profile> -- /bin/true` in
/// the sidecar's own container, which shares the pod's seccomp settings
/// and the node's kernel with the agent container. nono's own exit 1 with
/// `Landlock not available` is the `SandboxUnavailable` signal (§19.2);
/// `/sys/kernel/security/lsm` is not mounted in a pod, so there is nothing
/// else to read.
pub fn sandbox_self_test(tools: &ToolPaths, paths: &AgentPaths) -> Result<(), SelfTestError> {
    let out = std::process::Command::new(&tools.nono)
        .args(["-s", "run", "--profile"])
        .arg(&paths.profile)
        .args(["--", "/bin/true"])
        .env_clear()
        .env("HOME", &paths.nono_home)
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .output()
        .map_err(|e| SelfTestError::Io(e.to_string()))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let line = stderr.lines().next().unwrap_or("").trim().to_string();
    if out.status.code() == Some(1) && line.contains("Landlock not available") {
        return Err(SelfTestError::Unavailable(line));
    }
    Err(SelfTestError::Failed(line))
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelfTestError {
    /// nono exited 1 with `Landlock not available` (Spec O §19.2).
    #[error("SandboxUnavailable: {0}")]
    Unavailable(String),
    /// Any other non-zero exit; the first stderr line.
    #[error("sandbox self-test failed: {0}")]
    Failed(String),
    #[error("sandbox self-test could not run nono: {0}")]
    Io(String),
}
```

Check how `validate_profile` (`sandbox.rs:372-410`) sets nono's environment and copy its `HOME`/`PATH` handling exactly if it differs from the above (nono's state root follows its own `$HOME`, which must be `nono_home`, never the agent's `home/`: ARCHITECTURE.md). Export from `lib.rs`: add `SelfTestError, sandbox_self_test` to the `pub use sandbox::{…}` list.

- [ ] **Step 5: Run the pod tests and the whole runtime tier**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo test -p balerix-runtime --test materialize_pod_it` then `mise run test-it`
Expected: 3 passed; the existing `workspace_it` and `materialize_it` still pass (one-machine `create_clone` still fetches: `cache_is_read_only` is false there).

- [ ] **Step 6: Gate and commit**

Run: `mise run check`

```bash
git add crates/balerix-runtime
git commit -m "feat(runtime): materialise in a pod over the read-only crew slice; the sandbox self-test (Spec O §6.2, §8.1)"
```

---

### Task 5: `balerix-runtime`: `TmuxRunner` on a socket path, `-u`, and pod-mode waits

**Files:**
- Modify: `crates/balerix-runtime/src/tmux.rs:33-54, 155-175, 201-203, 270-287, 396-553, 651-726, 139-153`
- Create: `crates/balerix-runtime/tests/tmux_pod_it.rs`

**Interfaces:**
- Consumes: `TmuxRunner::new(tmux, name)`, `wait_gone`, `respawn_idle`, `windows`, `parse_windows`, `WINDOW_FORMAT`, `TmuxAttach` (existing).
- Produces, for Tasks 12–13:
  - `TmuxRunner::at_socket(tmux: PathBuf, socket: PathBuf) -> Self`: every client call is `tmux -S <socket> -u …`; process waits use `pane_dead`.
  - `pub socket_path: Option<PathBuf>` on `TmuxRunner`; `pub fn pod(&self) -> bool`.
  - Unchanged: `AgentRunner` semantics, `STOP_WAIT`, `RunnerError::StillRunning { id, pid }` with the pane's pid.
  - A pod-mode `stop_agent` ends with the window gone (the agent is *absent* to `observe`); a pod-mode restart (`ensure_agent` on `Running`) waits for the old pane to be dead before the script respawns.

- [ ] **Step 1: Write the failing integration tests**

Create `crates/balerix-runtime/tests/tmux_pod_it.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §6.3: the runner the sidecar uses. A socket *path* on a shared
//! directory, `-u` on every client call, and waits that never read
//! `/proc` (the pane's pid is another container's).
mod support;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use balerix_core::{AgentId, AgentRunner, LaunchPlan, ProcessState};
use balerix_runtime::TmuxRunner;
use balerix_runtime::testing::pid_alive;

struct KillServer {
    tmux: PathBuf,
    socket: PathBuf,
}

impl Drop for KillServer {
    fn drop(&mut self) {
        let _ = std::process::Command::new(&self.tmux)
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .status();
    }
}

struct Pane {
    r: TmuxRunner,
    id: AgentId,
    plan: LaunchPlan,
    socket: PathBuf,
    tmux: PathBuf,
    _server: KillServer,
    _root: balerix_runtime::testing::TempRoot,
}

/// A pane process that takes half a second to die after the hangup: a
/// stand-in for the supervisor emptying its tree (as `tmux_it` uses).
const SLOW_TO_DIE: &str = "trap 'sleep 0.5; exit 0' HUP\nwhile :; do sleep 0.1; done";

fn pane(label: &str, body: &str) -> Option<Pane> {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("tmux", false));
        return None;
    };
    let root = support::temp_root(label);
    let socket = root.join("run").join("tmux.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let guard = KillServer {
        tmux: tools.tmux.clone(),
        socket: socket.clone(),
    };
    let r = TmuxRunner::at_socket(tools.tmux.clone(), socket.clone());
    let id: AgentId = "f/c/a".parse().unwrap();
    let agent_dir = root.join("agent");
    std::fs::create_dir_all(agent_dir.join("logs")).unwrap();
    let script = agent_dir.join("launch.sh");
    std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = LaunchPlan {
        cwd: agent_dir,
        env: BTreeMap::new(),
        argv: vec![],
        script,
    };
    r.ensure_crew(&id.crew_ref()).unwrap();
    r.ensure_agent(&id, &plan).unwrap();
    Some(Pane {
        r,
        id,
        plan,
        socket,
        tmux: tools.tmux.clone(),
        _server: guard,
        _root: root,
    })
}

fn running_pid(p: &Pane) -> u32 {
    match p.r.observe(&p.id.fleet).unwrap().get(&p.id) {
        Some(ProcessState::Running { pid }) => *pid,
        other => panic!("expected running, got {other:?}"),
    }
}

fn tmux(p: &Pane, args: &[&str]) -> String {
    let out = std::process::Command::new(&p.tmux)
        .arg("-S")
        .arg(&p.socket)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "tmux {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_runner_speaks_to_a_socket_path_and_is_a_pod_runner() {
    let Some(p) = pane("podsock", "exec sleep 300") else {
        return;
    };
    assert!(p.r.pod());
    assert!(p.socket.exists(), "the server listens on the given path");
    assert_eq!(tmux(&p, &["list-windows", "-F", "#{window_name}"]).trim(), "balerix\na");
    assert!(matches!(
        p.r.observe(&p.id.fleet).unwrap().get(&p.id),
        Some(ProcessState::Running { .. })
    ));
}

/// Review Focus 1: with no UTF-8 locale a client without `-u` prints the
/// tabs of `WINDOW_FORMAT` as `_` (§19.1). The runner passes `-u`, so
/// `observe` parses whatever the environment is.
#[test]
fn observe_parses_without_a_utf8_locale() {
    let Some(p) = pane("podlocale", "exec sleep 300") else {
        return;
    };
    // the probe of the bug itself: a plain client in a C locale
    let out = std::process::Command::new(&p.tmux)
        .arg("-S")
        .arg(&p.socket)
        .args(["list-windows", "-F", "#{window_name}\t#{pane_dead}"])
        .env_remove("LANG")
        .env_remove("LC_ALL")
        .env_remove("LC_CTYPE")
        .env("LANG", "C")
        .output()
        .unwrap();
    let plain = String::from_utf8_lossy(&out.stdout);
    let with_u = std::process::Command::new(&p.tmux)
        .arg("-S")
        .arg(&p.socket)
        .args(["-u", "list-windows", "-F", "#{window_name}\t#{pane_dead}"])
        .env_remove("LANG")
        .env_remove("LC_ALL")
        .env_remove("LC_CTYPE")
        .env("LANG", "C")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&with_u.stdout).contains('\t'),
        "-u keeps the tab"
    );
    // whether or not this tmux build mangles tabs in a C locale (3.7c on
    // the spike did), the runner's own parse must hold
    eprintln!("plain client printed {plain:?}");
    assert_eq!(running_pid(&p) > 0, true);
}

/// The pod-mode stop: respawn into a waiter, wait for `pane_dead`, then
/// kill the window. The agent is absent afterwards, as on one machine.
#[test]
fn stop_waits_for_the_pane_to_die_then_removes_the_window() {
    let Some(p) = pane("podstop", SLOW_TO_DIE) else {
        return;
    };
    let pid = running_pid(&p);
    let start = Instant::now();
    p.r.stop_agent(&p.id).unwrap();
    assert!(
        start.elapsed() >= Duration::from_millis(450),
        "stop returned before the pane process could have died: {:?}",
        start.elapsed()
    );
    assert!(!pid_alive(pid), "the pane process outlived stop_agent");
    assert_eq!(p.r.observe(&p.id.fleet).unwrap().get(&p.id), None, "absent after stop");
    p.r.stop_agent(&p.id).unwrap(); // absent → ok
    // a pane that already exited stops at once
    p.r.ensure_agent(&p.id, &p.plan).unwrap();
    let pid = running_pid(&p);
    assert!(
        std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let t = Instant::now();
    while !matches!(
        p.r.observe(&p.id.fleet).unwrap().get(&p.id),
        Some(ProcessState::Exited { .. })
    ) {
        assert!(t.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(50));
    }
    let start = Instant::now();
    p.r.stop_agent(&p.id).unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(p.r.observe(&p.id.fleet).unwrap().get(&p.id), None);
}

/// A restart waits for the old pane process the same way, and keeps the
/// window.
#[test]
fn a_restart_waits_for_the_old_pane_to_die() {
    let Some(p) = pane("podrestart", SLOW_TO_DIE) else {
        return;
    };
    let old = running_pid(&p);
    p.r.ensure_agent(&p.id, &p.plan).unwrap();
    assert!(!pid_alive(old), "the old pane process outlived the restart");
    assert_ne!(running_pid(&p), old);
}

/// Review Focus 2: past the bound the stop fails and names the pid, and
/// the window is still there with its pane alive (the spec's rule: the
/// window is not removed before the pane is dead).
#[test]
fn a_pane_process_that_ignores_the_hangup_fails_the_stop_and_keeps_the_window() {
    let Some(mut p) = pane("podbound", "trap '' HUP\nwhile :; do sleep 0.2; done") else {
        return;
    };
    p.r.stop_wait = Duration::from_millis(300);
    let pid = running_pid(&p);
    let start = Instant::now();
    let err = p.r.stop_agent(&p.id).unwrap_err();
    assert!(start.elapsed() >= Duration::from_millis(300));
    assert_eq!(
        err.to_string(),
        format!("f/c/a: agent processes still running after stop (pid {pid})")
    );
    assert!(pid_alive(pid));
    assert!(
        tmux(&p, &["list-windows", "-F", "#{window_name}"]).contains('a'),
        "the window stays until the pane is dead"
    );
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
}

#[test]
fn stop_crew_ends_every_window_and_the_session() {
    let Some(p) = pane("podstopcrew", SLOW_TO_DIE) else {
        return;
    };
    let pid = running_pid(&p);
    p.r.stop_crew(&p.id.crew_ref()).unwrap();
    assert!(!pid_alive(pid));
    assert!(p.r.observe(&p.id.fleet).unwrap().crews.is_empty());
    p.r.stop_crew(&p.id.crew_ref()).unwrap(); // absent → ok
}

#[test]
fn attach_works_over_the_socket_path() {
    let Some(p) = pane("podattach", "exec sleep 300") else {
        return;
    };
    let stream = p.r.attach(&p.id).unwrap();
    let mut reader = stream.reader().unwrap();
    let mut buf = [0u8; 1024];
    // the grouped session draws the pane; any bytes prove the PTY is up
    let n = reader.read(&mut buf).unwrap();
    assert!(n > 0);
    drop(stream);
    let t = Instant::now();
    loop {
        let sessions = tmux(&p, &["list-sessions", "-F", "#{session_name}"]);
        if !sessions.contains("balerix-attach-") {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(5), "the attach session lingers");
        std::thread::sleep(Duration::from_millis(50));
    }
}
```

Add `use std::io::Read;` at the top for the attach test.

- [ ] **Step 2: Run to see them fail**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo test -p balerix-runtime --test tmux_pod_it`
Expected: compile error (`at_socket`, `pod` undefined).

- [ ] **Step 3: The socket path, `-u`, and the pod-mode waits**

In `crates/balerix-runtime/src/tmux.rs`:

Struct and constructors (replace lines 155-175):

```rust
pub struct TmuxRunner {
    pub tmux: PathBuf,
    /// The `-L` socket name (one machine).
    pub socket: String,
    /// Spec O §6.3: a `-S` socket path on the pod's run directory. Set,
    /// the runner is a pod runner: every client call carries `-u`, and
    /// the waits on a pane's process go through tmux's `pane_dead`, never
    /// `/proc` (the pid is the agent container's).
    pub socket_path: Option<PathBuf>,
    /// `STOP_WAIT`; a field so a test can shorten it.
    pub stop_wait: Duration,
    sends: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl TmuxRunner {
    pub fn new(tmux: PathBuf, socket: impl Into<String>) -> Self {
        Self {
            tmux,
            socket: socket.into(),
            socket_path: None,
            stop_wait: STOP_WAIT,
            sends: Mutex::new(HashMap::new()),
        }
    }

    /// The sidecar's runner (Spec O §6.3).
    pub fn at_socket(tmux: PathBuf, socket: PathBuf) -> Self {
        Self {
            tmux,
            socket: String::new(),
            socket_path: Some(socket),
            stop_wait: STOP_WAIT,
            sends: Mutex::new(HashMap::new()),
        }
    }

    pub fn pod(&self) -> bool {
        self.socket_path.is_some()
    }

    /// `-S <path> -u` in a pod, `-L <name>` on one machine.
    pub fn socket_args(&self) -> Vec<String> {
        match &self.socket_path {
            Some(p) => vec!["-S".to_string(), p.display().to_string(), "-u".to_string()],
            None => vec!["-L".to_string(), self.socket.clone()],
        }
    }
```

`cmd()` becomes `Cmd::new(&self.tmux).args(self.socket_args())`. `TmuxAttach` (lines 106-115) replaces its `socket: String` field with `socket_args: Vec<String>`; its `Drop` runs `Command::new(&self.tmux).args(&self.socket_args).args(["kill-session", "-t", &format!("={}", self.session)])`; `attach` (line ~690) builds the client's argv from `self.socket_args()` instead of `["-L", &self.socket]` and fills the new field.

The waits. Add after `wait_gone`:

```rust
    /// The pod-mode wait (Spec O §6.3): the pane's process runs in the
    /// agent container, so its pid cannot be watched from here. Polls
    /// `pane_dead` of the window until tmux reports the pane dead or the
    /// window is gone, within `stop_wait`; past it, `StillRunning` with
    /// the pid tmux reported.
    fn wait_pane_dead(&self, id: &str, agent: &AgentId, pid: u32) -> Result<(), RunnerError> {
        let deadline = Instant::now() + self.stop_wait;
        loop {
            match self
                .windows(&agent.crew_ref())?
                .and_then(|w| w.get(&agent.agent).copied())
            {
                None | Some(ProcessState::Exited { .. }) => return Ok(()),
                Some(ProcessState::Running { .. }) => {}
            }
            if Instant::now() >= deadline {
                return Err(RunnerError::StillRunning {
                    id: id.to_string(),
                    pid,
                });
            }
            std::thread::sleep(STOP_POLL);
        }
    }

    /// Pod mode's way of ending a pane's process while keeping its window:
    /// `respawn-window -k` hangs the process up exactly as `kill-window`
    /// would and starts, in the agent container, a command that lives
    /// while that process does. The supervisor exits only when its tree
    /// is empty; the waiter exits when the supervisor is gone; the pane is
    /// then dead to `windows()`. `kill -0` of a reused pid would hold the
    /// waiter until the bound: `StillRunning`, as for a tree that will not
    /// die.
    fn respawn_into_waiter(
        &self,
        id: &str,
        target: &str,
        cwd: &str,
        pid: u32,
    ) -> Result<(), RunnerError> {
        let waiter = format!("while kill -0 {pid} 2>/dev/null; do sleep 0.02; done");
        self.run(
            id,
            &[
                "respawn-window",
                "-k",
                "-t",
                target,
                "-c",
                cwd,
                "/bin/sh",
                "-c",
                &waiter,
            ],
        )
        .map(|_| ())
    }
```

`ensure_agent`'s `Running` arm becomes:

```rust
            Some(ProcessState::Running { pid }) => {
                // A restart: end the old process first and wait for it,
                // so the new agent never starts while the old tree is
                // still dying (Spec N amendment §13.5). On one machine
                // through the idle placeholder and `/proc`; in a pod
                // (Spec O §6.3) through a waiter and `pane_dead`.
                if self.pod() {
                    self.respawn_into_waiter(&id, &target, &cwd, pid)?;
                    self.wait_pane_dead(&id, agent, pid)?;
                    // the pane is dead now and `pipe-pane` refuses a dead
                    // pane: revive it into the idle placeholder, as the
                    // `Exited` arm does
                    self.respawn_idle(&id, &target, &cwd)?;
                } else {
                    let old = ProcIdentity::of(pid);
                    self.respawn_idle(&id, &target, &cwd)?;
                    self.wait_gone(&id, old.as_slice())?;
                }
            }
```

(After the pod arm the pane holds the idle placeholder, alive; the common path's `set-option`, `pipe-pane` and `respawn-window -k <script>` follow as for the other arms.)

`stop_agent`:

```rust
    fn stop_agent(&self, agent: &AgentId) -> Result<(), RunnerError> {
        let id = agent.to_string();
        let state = self
            .windows(&agent.crew_ref())?
            .and_then(|w| w.get(&agent.agent).copied());
        if self.pod() {
            // Spec O §6.3: the window stays until the pane is dead, so
            // `pane_dead` can be waited on; then it goes, so the agent is
            // absent, as it is on one machine after `kill-window`.
            if let Some(ProcessState::Running { pid }) = state {
                let cwd = "/";
                self.respawn_into_waiter(&id, &Self::window_target(agent), cwd, pid)?;
                self.wait_pane_dead(&id, agent, pid)?;
            }
            self.run_optional(&id, &["kill-window", "-t", &Self::window_target(agent)])?;
            return Ok(());
        }
        let pane = match state {
            Some(ProcessState::Running { pid }) => ProcIdentity::of(pid),
            _ => None,
        };
        self.run_optional(&id, &["kill-window", "-t", &Self::window_target(agent)])?;
        self.wait_gone(&id, pane.as_slice())
    }
```

`stop_crew` in pod mode: before the kills, for every agent window of the crew in `Running` run `respawn_into_waiter` + `wait_pane_dead` (collect the first error, still kill the sessions, then return it); the anchor's idle shell needs no wait (its tree is one `sh`). Skip the `list-panes`/`ProcIdentity` collection and the final `wait_gone` when `self.pod()`:

```rust
        if self.pod() {
            let mut first_err = None;
            if let Some(windows) = self.windows(crew)? {
                for (agent, state) in windows {
                    if let ProcessState::Running { pid } = state {
                        let id = AgentId {
                            fleet: crew.fleet.clone(),
                            crew: crew.crew.clone(),
                            agent,
                        };
                        let target = Self::window_target(&id);
                        let r = self
                            .respawn_into_waiter(&id.to_string(), &target, "/", pid)
                            .and_then(|()| self.wait_pane_dead(&id.to_string(), &id, pid));
                        if let Err(e) = r
                            && first_err.is_none()
                        {
                            first_err = Some(e);
                        }
                    }
                }
            }
            for session in &sessions {
                self.run_optional(&name, &["kill-session", "-t", &format!("={session}")])?;
            }
            return first_err.map_or(Ok(()), Err);
        }
```
placed right after `sessions` is computed and before the `panes` collection.

- [ ] **Step 4: Run the pod tests, then the whole runtime tier**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo test -p balerix-runtime --test tmux_pod_it` then `mise run test-it`
Expected: 7 passed; `tmux_it` unchanged (one-machine paths untouched).

- [ ] **Step 5: Gate and commit**

Run: `mise run check`

```bash
git add crates/balerix-runtime
git commit -m "feat(runtime): TmuxRunner on a socket path with -u; pod-mode waits through pane_dead, never /proc (Spec O §6.3)"
```

---

### Task 6: `balerix-server`: the link hub

**Files:**
- Modify: `crates/balerix-core/src/ports.rs:126-141` (`RunnerError::Link`)
- Create: `crates/balerix-server/src/kube/mod.rs`
- Create: `crates/balerix-server/src/kube/link.rs`
- Modify: `crates/balerix-server/src/lib.rs:6-24, 26-45`
- Modify: `crates/balerix-server/src/actor.rs:33-57, 65-84, 189-236, 340-345`
- Modify: `crates/balerix-server/src/daemon.rs:321-327` (accessors)
- Modify: `crates/balerix-server/src/api.rs:153-158` (the link route)
- Modify: `crates/balerix-server/src/testing.rs:60-122` (`Harness::kube`)
- Modify: `crates/balerix/src/commands/serve.rs:162-173` (`kube: None`)
- Create: `crates/balerix-server/tests/kube_link_it.rs`

**Interfaces:**
- Consumes: `balerix_api::{LinkRequest, LinkOp, LinkReply, LinkResult, LinkFailure, FailureKind, LinkStatus, SidecarFrame, LINK_PROTOCOL, LINK_PROTOCOL_HEADER}` (Task 2); `AgentRunner`, `WorkspaceReader`, `RunnerError`, `WorkspaceError`, `ObservedState`, `ProcessState` (core); `Daemon::verify_secret`, `auth::bearer`, `FleetHandle.tx` (existing).
- Produces, for Tasks 7–9 and the sidecar (Tasks 12–13):
  - `balerix_core::RunnerError::Link { id: String, message: String }`, displayed `"{id}: {message}"`.
  - `balerix_server::kube::{LinkHub, LinkError, LINK_CALL_TIMEOUT}`: `LinkHub::new() -> Arc<LinkHub>`; `linked(&AgentId) -> bool`; `call(&AgentId, LinkOp) -> Result<LinkResult, LinkError>` (**blocking**: call it from `spawn_blocking`, as every port method is); `serve(self: Arc<Self>, daemon: Arc<Daemon>, agent: AgentId, socket: WebSocket)` (the route's upgrade body); `impl AgentRunner for LinkHub`; `impl WorkspaceReader for LinkHub`.
  - `Ports.kube: Option<Arc<LinkHub>>` (`None` on one machine).
  - `Msg::LinkStatus { agent: AgentId, status: LinkStatus }`, `Msg::LinkDown { agent: AgentId }`; `Daemon::kube(&self) -> Option<&Arc<LinkHub>>`, `Daemon::link_status(&self, &AgentId, LinkStatus)`, `Daemon::link_down(&self, &AgentId)`.
  - Route `GET /v1/agents/{fleet}/{crew}/{agent}/link` (WebSocket): `Authorization: Bearer <agent secret>`, header `balerix-link-protocol: 1`; 404 `not a daemon in kubernetes mode` without `Ports.kube`; 401 `unknown agent or bad secret`; 400 `balerix-link-protocol: this daemon speaks link protocol 1, got <v|nothing>`.
  - `Harness::kube(resync: Duration) -> Harness` with `pub hub: Option<Arc<LinkHub>>`.
  - In Kubernetes mode an actor's `pass()` runs no planner: it recomputes the fleet phase from the mirrored agents (`finish_pass`).

- [ ] **Step 1: Write the failing integration test**

Create `crates/balerix-server/tests/kube_link_it.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §7.2: the Daemon's end of the sidecar link, with a fake sidecar
//! on a tungstenite client.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use balerix_api::{
    AgentPhase, AgentSettings, AgentStatus, CredentialBundle, CrewSpec, FleetSpec,
    LINK_PROTOCOL_HEADER, LinkOp, LinkReply, LinkRequest, LinkResult, LinkStatus, SidecarFrame,
    FailureKind, LinkFailure,
};
use balerix_core::{AgentId, AgentRunner, PassThrough, ProcessState, RunnerError, WorkspaceError, WorkspaceReader};
use balerix_server::testing::Harness;
use balerix_server::{Daemon, router, serve};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

fn spec() -> FleetSpec {
    FleetSpec {
        name: "f".into(),
        tools: BTreeMap::new(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/api".into(),
                git_ref: "main".into(),
                agents: BTreeMap::from([("a".to_string(), AgentSettings::default())]),
                ..CrewSpec::default()
            },
        )]),
    }
}

struct World {
    daemon: Arc<Daemon>,
    port: u16,
    _dir: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

async fn world(h: &Harness) -> World {
    let dir = tempfile::tempdir().unwrap();
    let daemon = h.daemon(Arc::new(PassThrough), dir.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve(listener, router(daemon.clone()), async {
        let _ = rx.await;
    }));
    World {
        daemon,
        port,
        _dir: dir,
        stop: Some(stop),
    }
}

async fn connect(
    port: u16,
    token: &str,
    protocol: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::Error,
> {
    let mut req = format!("ws://127.0.0.1:{port}/v1/agents/f/c/a/link")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    if !protocol.is_empty() {
        req.headers_mut()
            .insert(LINK_PROTOCOL_HEADER, protocol.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req).await.map(|(ws, _)| ws)
}

async fn wait_for(mut f: impl AsyncFnMut() -> bool) {
    let start = Instant::now();
    while !f().await {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn text(frame: &SidecarFrame) -> Message {
    Message::Text(serde_json::to_string(frame).unwrap().into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sidecar_links_sends_status_and_answers_calls() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world(&h).await;
    let name = "f".parse().unwrap();
    w.daemon
        .apply(&name, spec(), CredentialBundle::default(), false)
        .await
        .unwrap();
    let id: AgentId = "f/c/a".parse().unwrap();
    let token = w.daemon.hook_secret(&id).await.unwrap();
    let hub = w.daemon.kube().unwrap().clone();
    assert!(!hub.linked(&id));

    let mut ws = connect(w.port, &token, "1").await.unwrap();
    wait_for(async || hub.linked(&id)).await;

    // a status frame is mirrored into the record
    ws.send(text(&SidecarFrame::Status(LinkStatus {
        status: AgentStatus {
            phase: AgentPhase::Ready,
            restarts: 2,
            ..AgentStatus::default()
        },
        pid: Some(56),
        hook_failures: 0,
    })))
    .await
    .unwrap();
    wait_for(async || {
        let r = w.daemon.get(&name).await.unwrap();
        r.status.agents.get("f/c/a").is_some_and(|a| a.phase == AgentPhase::Ready && a.restarts == 2)
    })
    .await;
    assert_eq!(
        hub.observe(&name).unwrap().get(&id),
        Some(&ProcessState::Running { pid: 56 }),
        "observe answers from the last status frame"
    );
    assert_eq!(
        w.daemon.get(&name).await.unwrap().status.phase,
        balerix_api::FleetPhase::Ready,
        "the fleet phase follows the mirrored agents"
    );

    // a call goes out as a request and its reply comes back to the caller
    let (hub2, id2) = (hub.clone(), id.clone());
    let call = tokio::task::spawn_blocking(move || hub2.send_text(&id2, "hello", true));
    let frame = ws.next().await.unwrap().unwrap().into_text().unwrap();
    let req: LinkRequest = serde_json::from_str(frame.as_str()).unwrap();
    assert_eq!(
        req.op,
        LinkOp::SendText {
            text: "hello".into(),
            submit: true
        }
    );
    ws.send(text(&SidecarFrame::Reply(LinkReply {
        id: req.id,
        result: LinkResult::Ok,
    })))
    .await
    .unwrap();
    call.await.unwrap().unwrap();

    // a failure is rebuilt as the port's error
    let (hub2, id2) = (hub.clone(), id.clone());
    let call = tokio::task::spawn_blocking(move || hub2.read_file(&id2, "nope"));
    let frame = ws.next().await.unwrap().unwrap().into_text().unwrap();
    let req: LinkRequest = serde_json::from_str(frame.as_str()).unwrap();
    assert_eq!(
        req.op,
        LinkOp::WorkspaceFile {
            path: "nope".into()
        }
    );
    ws.send(text(&SidecarFrame::Reply(LinkReply {
        id: req.id,
        result: LinkResult::Failed {
            failure: LinkFailure {
                reason: FailureKind::NoSuchPath,
                message: "no such path".into(),
            },
        },
    })))
    .await
    .unwrap();
    assert_eq!(call.await.unwrap(), Err(WorkspaceError::NoSuchPath));

    // the link closes: calls fail naming it, the record says so
    ws.close(None).await.unwrap();
    wait_for(async || !hub.linked(&id)).await;
    let (hub2, id2) = (hub.clone(), id.clone());
    let err = tokio::task::spawn_blocking(move || hub2.send_text(&id2, "x", false))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        err,
        RunnerError::Link {
            id: "f/c/a".into(),
            message: "link down".into()
        }
    );
    assert_eq!(err.to_string(), "f/c/a: link down");
    wait_for(async || {
        w.daemon
            .get(&name)
            .await
            .unwrap()
            .status
            .agents["f/c/a"]
            .message
            == "link down"
    })
    .await;
    assert_eq!(hub.observe(&name).unwrap().get(&id), None, "absent without a link");

    // a reconnect replaces the old connection
    let _ws = connect(w.port, &token, "1").await.unwrap();
    wait_for(async || hub.linked(&id)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_link_route_refuses_a_bad_token_a_wrong_protocol_and_a_tmux_daemon() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world(&h).await;
    let name = "f".parse().unwrap();
    w.daemon
        .apply(&name, spec(), CredentialBundle::default(), false)
        .await
        .unwrap();
    let id: AgentId = "f/c/a".parse().unwrap();
    let token = w.daemon.hook_secret(&id).await.unwrap();
    let status = |e: tokio_tungstenite::tungstenite::Error| match e {
        tokio_tungstenite::tungstenite::Error::Http(r) => r.status().as_u16(),
        other => panic!("{other:?}"),
    };
    assert_eq!(status(connect(w.port, "wrong", "1").await.unwrap_err()), 401);
    assert_eq!(status(connect(w.port, &token, "2").await.unwrap_err()), 400);
    assert_eq!(status(connect(w.port, &token, "").await.unwrap_err()), 400);

    let tmux = Harness::new(Duration::from_secs(3600));
    let w2 = world(&tmux).await;
    w2.daemon
        .apply(&name, spec(), CredentialBundle::default(), false)
        .await
        .unwrap();
    let token = w2.daemon.hook_secret(&id).await.unwrap();
    assert_eq!(status(connect(w2.port, &token, "1").await.unwrap_err()), 404);
}
```

`AsyncFnMut` closures (`async || …`) are stable on Rust 1.85+; the toolchain is 1.99.0.

- [ ] **Step 2: Run to see it fail**

Run: `mise x -- cargo test -p balerix-server --test kube_link_it`
Expected: compile errors (`Harness::kube`, `Daemon::kube`, `RunnerError::Link`).

- [ ] **Step 3: `RunnerError::Link`**

In `crates/balerix-core/src/ports.rs`, add the variant to `RunnerError` (after `StillRunning`):

```rust
    /// Spec O §7.2: the agent's sidecar link is down, timed out, or the
    /// sidecar answered with a failure.
    #[error("{id}: {message}")]
    Link { id: String, message: String },
```

`cargo mutants` excludes nothing here; the variant is constructed only by the server, so no core test changes.

- [ ] **Step 4: The hub**

Create `crates/balerix-server/src/kube/mod.rs`:

```rust
//! Kubernetes mode (Spec O §7): the sidecar link and the two ports over
//! it. The operator's apply, the idle ports and TLS serving join this
//! module in later tasks.

pub mod link;

pub use link::{LINK_CALL_TIMEOUT, LinkError, LinkHub};

/// `FleetRecord.owner` of a fleet the operator applied (§7.3).
pub const KUBERNETES_OWNER: &str = "kubernetes";
```

Create `crates/balerix-server/src/kube/link.rs`:

```rust
//! The Daemon's end of the sidecar link (Spec O §7.2). One WebSocket per
//! agent, opened by the sidecar at `GET /v1/agents/{f}/{c}/{a}/link` with
//! the agent's token. Requests leave as `LinkRequest` text frames, each
//! answered by a `LinkReply` carrying the same id; the sidecar's `status`
//! frames go to the fleet actor. `LinkHub` implements the two sync ports
//! over the link, so `execute_action` and the plugin routes work unchanged;
//! a call for an agent whose link is down fails naming it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::PathRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use balerix_api::{
    FailureKind, LINK_PROTOCOL, LINK_PROTOCOL_HEADER, LinkFailure, LinkOp, LinkRequest,
    LinkResult, LinkStatus, SidecarFrame, WorkspaceDiff, WorkspaceTree, WorkspaceVersion,
};
use balerix_core::{
    AgentId, AgentRunner, CrewRef, FleetName, LaunchPlan, ObservedState, ProcessState, PtyStream,
    RunnerError, WorkspaceError, WorkspaceReader,
};
use tokio::sync::mpsc;

use crate::api::{ApiError, AppState};
use crate::auth::bearer;
use crate::daemon::Daemon;

/// How long a blocking call waits for the sidecar's reply.
pub const LINK_CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// Reaps a sidecar that vanished without a close frame (as `watch.rs`).
const PING_INTERVAL: Duration = Duration::from_secs(30);

type Pending = Arc<Mutex<HashMap<u64, SyncSender<LinkResult>>>>;

struct Conn {
    tx: mpsc::UnboundedSender<LinkRequest>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    /// Which connection this is: a reconnect replaces an older one, and
    /// the older one's teardown must not unregister the newer.
    epoch: u64,
    /// The last `status` frame, for `observe`.
    last: Option<LinkStatus>,
}

#[derive(Default)]
pub struct LinkHub {
    conns: Mutex<HashMap<AgentId, Conn>>,
    epochs: AtomicU64,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LinkError {
    #[error("link down")]
    Down,
    #[error("the sidecar did not answer within {} s", LINK_CALL_TIMEOUT.as_secs())]
    Timeout,
    #[error("{}", .0.message)]
    Failed(LinkFailure),
    #[error("unexpected reply to {0}")]
    Unexpected(&'static str),
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl LinkHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn linked(&self, agent: &AgentId) -> bool {
        lock(&self.conns).contains_key(agent)
    }

    fn register(&self, agent: &AgentId, tx: mpsc::UnboundedSender<LinkRequest>, pending: Pending) -> u64 {
        let epoch = self.epochs.fetch_add(1, Ordering::SeqCst);
        // an older connection's pending calls fail with `Down` when their
        // senders drop here
        lock(&self.conns).insert(
            agent.clone(),
            Conn {
                tx,
                pending,
                next_id: Arc::new(AtomicU64::new(1)),
                epoch,
                last: None,
            },
        );
        epoch
    }

    fn unregister(&self, agent: &AgentId, epoch: u64) {
        let mut conns = lock(&self.conns);
        if conns.get(agent).is_some_and(|c| c.epoch == epoch) {
            conns.remove(agent);
        }
    }

    fn note_status(&self, agent: &AgentId, epoch: u64, status: &LinkStatus) {
        if let Some(c) = lock(&self.conns).get_mut(agent)
            && c.epoch == epoch
        {
            c.last = Some(status.clone());
        }
    }

    /// One request and its reply. Blocking: every port method is, and the
    /// daemon calls them in `spawn_blocking`; never call this on a tokio
    /// worker.
    pub fn call(&self, agent: &AgentId, op: LinkOp) -> Result<LinkResult, LinkError> {
        let (tx, rx) = sync_channel(1);
        let (sender, pending, id) = {
            let conns = lock(&self.conns);
            let c = conns.get(agent).ok_or(LinkError::Down)?;
            let id = c.next_id.fetch_add(1, Ordering::SeqCst);
            lock(&c.pending).insert(id, tx);
            (c.tx.clone(), c.pending.clone(), id)
        };
        if sender.send(LinkRequest { id, op }).is_err() {
            lock(&pending).remove(&id);
            return Err(LinkError::Down);
        }
        match rx.recv_timeout(LINK_CALL_TIMEOUT) {
            Ok(LinkResult::Failed { failure }) => Err(LinkError::Failed(failure)),
            Ok(r) => Ok(r),
            Err(RecvTimeoutError::Timeout) => {
                lock(&pending).remove(&id);
                Err(LinkError::Timeout)
            }
            Err(RecvTimeoutError::Disconnected) => Err(LinkError::Down),
        }
    }

    /// The route's upgrade body: registers the link, pumps frames both
    /// ways, pings, and on the socket's end tells the daemon.
    pub async fn serve(self: Arc<Self>, daemon: Arc<Daemon>, agent: AgentId, mut socket: WebSocket) {
        let (tx, mut rx) = mpsc::unbounded_channel::<LinkRequest>();
        let pending: Pending = Arc::default();
        let epoch = self.register(&agent, tx, pending.clone());
        tracing::info!(agent = %agent, "sidecar linked");
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await; // the first tick is immediate
        loop {
            tokio::select! {
                req = rx.recv() => match req {
                    Some(req) => {
                        let Ok(json) = serde_json::to_string(&req) else { break };
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                _ = ping.tick() => {
                    if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                        break;
                    }
                }
                msg = socket.recv() => match msg {
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<SidecarFrame>(text.as_str()) {
                        Ok(SidecarFrame::Status(status)) => {
                            self.note_status(&agent, epoch, &status);
                            daemon.link_status(&agent, status).await;
                        }
                        Ok(SidecarFrame::Reply(reply)) => {
                            if let Some(tx) = lock(&pending).remove(&reply.id) {
                                // the caller may have timed out and gone
                                let _ = tx.try_send(reply.result);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(agent = %agent, "link: not a sidecar frame: {e}");
                            let _ = socket.send(Message::Close(Some(CloseFrame {
                                code: crate::attach::CLOSE_UNSUPPORTED,
                                reason: "expected a sidecar frame".into(),
                            }))).await;
                            break;
                        }
                    },
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                },
            }
        }
        self.unregister(&agent, epoch);
        // a replaced connection is not a link loss
        if !self.linked(&agent) {
            tracing::info!(agent = %agent, "sidecar link closed");
            daemon.link_down(&agent).await;
        }
    }
}

fn runner_err(agent: &AgentId, e: LinkError) -> RunnerError {
    RunnerError::Link {
        id: agent.to_string(),
        message: e.to_string(),
    }
}

fn workspace_err(agent: &AgentId, e: LinkError) -> WorkspaceError {
    match e {
        LinkError::Failed(LinkFailure { reason, message }) => match reason {
            FailureKind::Missing => WorkspaceError::Missing(message),
            FailureKind::NoSuchPath => WorkspaceError::NoSuchPath,
            FailureKind::InvalidPath => WorkspaceError::InvalidPath(message),
            FailureKind::NotAFile => WorkspaceError::NotAFile,
            FailureKind::NotADirectory => WorkspaceError::NotADirectory,
            FailureKind::TooLarge { limit } => WorkspaceError::TooLarge { limit },
            FailureKind::Tool => WorkspaceError::Tool {
                id: agent.to_string(),
                subcommand: "link".into(),
                args: vec![],
                stderr: message,
            },
            FailureKind::Filter => WorkspaceError::Filter { key: message },
            FailureKind::Io | FailureKind::Runner => WorkspaceError::Io {
                path: PathBuf::from("link"),
                message,
            },
        },
        other => WorkspaceError::Io {
            path: PathBuf::from("link"),
            message: format!("{agent}: {other}"),
        },
    }
}

const NOT_THE_DAEMONS: &str =
    "the daemon neither starts nor stops agents in kubernetes mode; the sidecar does (Spec O §7.2)";

impl AgentRunner for LinkHub {
    fn ensure_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        Err(RunnerError::Link {
            id: crew.to_string(),
            message: NOT_THE_DAEMONS.into(),
        })
    }
    fn ensure_agent(&self, agent: &AgentId, _plan: &LaunchPlan) -> Result<(), RunnerError> {
        Err(runner_err(agent, LinkError::Failed(LinkFailure {
            reason: FailureKind::Runner,
            message: NOT_THE_DAEMONS.into(),
        })))
    }
    fn stop_agent(&self, agent: &AgentId) -> Result<(), RunnerError> {
        self.ensure_agent(agent, &LaunchPlan {
            cwd: PathBuf::new(),
            env: Default::default(),
            argv: vec![],
            script: PathBuf::new(),
        })
    }
    fn stop_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        self.ensure_crew(crew)
    }
    /// Every linked agent of the fleet, from its last `status` frame: a pid
    /// is `Running`, no pid is `Exited`; an agent without a link is absent.
    fn observe(&self, fleet: &FleetName) -> Result<ObservedState, RunnerError> {
        let mut out = ObservedState::default();
        for (id, c) in lock(&self.conns).iter() {
            if id.fleet != *fleet {
                continue;
            }
            let state = match c.last.as_ref().and_then(|s| s.pid) {
                Some(pid) => ProcessState::Running { pid },
                None => ProcessState::Exited { code: None },
            };
            out.set(id, state);
        }
        Ok(out)
    }
    fn send_text(&self, agent: &AgentId, text: &str, submit: bool) -> Result<(), RunnerError> {
        self.call(agent, LinkOp::SendText {
            text: text.to_string(),
            submit,
        })
        .map(|_| ())
        .map_err(|e| runner_err(agent, e))
    }
    fn send_keys(
        &self,
        agent: &AgentId,
        steps: &[balerix_api::KeyStep],
        delay: Duration,
    ) -> Result<(), RunnerError> {
        self.call(agent, LinkOp::SendKeys {
            steps: steps.to_vec(),
            delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
        })
        .map(|_| ())
        .map_err(|e| runner_err(agent, e))
    }
    fn attach(&self, agent: &AgentId) -> Result<Box<dyn PtyStream>, RunnerError> {
        // Task 8 replaces this body with the second socket.
        Err(runner_err(agent, LinkError::Unexpected("attach")))
    }
}

impl WorkspaceReader for LinkHub {
    fn diff(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceDiff, WorkspaceError> {
        match self.call(agent, LinkOp::WorkspaceDiff { base_ref: base_ref.into() }) {
            Ok(LinkResult::Diff { diff }) => Ok(diff),
            Ok(_) => Err(workspace_err(agent, LinkError::Unexpected("workspace.diff"))),
            Err(e) => Err(workspace_err(agent, e)),
        }
    }
    fn read_file(&self, agent: &AgentId, path: &str) -> Result<Vec<u8>, WorkspaceError> {
        match self.call(agent, LinkOp::WorkspaceFile { path: path.into() }) {
            Ok(LinkResult::File { bytes }) => Ok(bytes),
            Ok(_) => Err(workspace_err(agent, LinkError::Unexpected("workspace.file"))),
            Err(e) => Err(workspace_err(agent, e)),
        }
    }
    fn list_dir(&self, agent: &AgentId, path: &str) -> Result<WorkspaceTree, WorkspaceError> {
        match self.call(agent, LinkOp::WorkspaceTree { path: path.into() }) {
            Ok(LinkResult::Tree { tree }) => Ok(tree),
            Ok(_) => Err(workspace_err(agent, LinkError::Unexpected("workspace.tree"))),
            Err(e) => Err(workspace_err(agent, e)),
        }
    }
    fn version(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceVersion, WorkspaceError> {
        match self.call(agent, LinkOp::WorkspaceVersion { base_ref: base_ref.into() }) {
            Ok(LinkResult::Version { version }) => Ok(version),
            Ok(_) => Err(workspace_err(agent, LinkError::Unexpected("workspace.version"))),
            Err(e) => Err(workspace_err(agent, e)),
        }
    }
}

/// `GET /v1/agents/{fleet}/{crew}/{agent}/link`: the sidecar's one socket.
/// Authenticated like the hook route (401 for a bad token or an unknown
/// agent alike), then the protocol header, then the upgrade.
pub(crate) async fn link(
    State(state): State<AppState>,
    path: Result<Path<(String, String, String)>, PathRejection>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(hub) = state.daemon.kube().cloned() else {
        return ApiError::new(StatusCode::NOT_FOUND, "not a daemon in kubernetes mode")
            .into_response();
    };
    let Path((fleet, crew, agent)) = match path {
        Ok(p) => p,
        Err(e) => return ApiError::new(e.status(), e.body_text()).into_response(),
    };
    let unauthorized =
        || ApiError::new(StatusCode::UNAUTHORIZED, "unknown agent or bad secret").into_response();
    let Ok(id) = format!("{fleet}/{crew}/{agent}").parse::<AgentId>() else {
        return unauthorized();
    };
    let Some(token) = bearer(&headers) else {
        return unauthorized();
    };
    if !state.daemon.verify_secret(&id, token).await {
        return unauthorized();
    }
    let got = headers
        .get(LINK_PROTOCOL_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u32>().ok());
    if got != Some(LINK_PROTOCOL) {
        return ApiError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "{LINK_PROTOCOL_HEADER}: this daemon speaks link protocol {LINK_PROTOCOL}, got {}",
                got.map_or("nothing".to_string(), |v| v.to_string())
            ),
        )
        .into_response();
    }
    let daemon = state.daemon.clone();
    ws.on_upgrade(move |socket| hub.serve(daemon, id, socket))
}
```

`ObservedState::set(&AgentId, ProcessState)` exists (`ports.rs`, section 5 of the core report). `ApiError::new` and `AppState` are `pub(crate)`-visible inside the crate (`api.rs:33-50`).

- [ ] **Step 5: Wire the hub into the actor, the daemon and the router**

`crates/balerix-server/src/lib.rs`: add `pub mod kube;` to the module list and `pub use kube::{KUBERNETES_OWNER, LINK_CALL_TIMEOUT, LinkError, LinkHub};`.

`crates/balerix-server/src/actor.rs`:
- `Msg` gains two variants:
  ```rust
      /// Spec O §7.2: a sidecar's `status` frame; the Daemon mirrors it.
      LinkStatus {
          agent: AgentId,
          status: balerix_api::LinkStatus,
      },
      /// The sidecar's link closed and no newer one replaced it.
      LinkDown { agent: AgentId },
  ```
- `Ports` gains `pub kube: Option<Arc<crate::kube::LinkHub>>,` with the doc `/// Spec O §7: set, this daemon is in Kubernetes mode: actors mirror sidecar status frames and run no planner; the runner and the workspace reader are the hub.`
- In `run()`'s `match msg`, two arms:
  ```rust
                  Some(Msg::LinkStatus { agent, status }) => self.link_status(agent, status).await,
                  Some(Msg::LinkDown { agent }) => {
                      if let Some(a) = self.record.status.agents.get_mut(&agent.to_string()) {
                          a.message = "link down".to_string();
                      }
                      self.publish();
                  }
  ```
- New methods on `Actor`:
  ```rust
      /// Spec O §7.2: the sidecar's status is the Daemon's observed state.
      async fn link_status(&mut self, agent: AgentId, status: balerix_api::LinkStatus) {
          self.record
              .status
              .agents
              .insert(agent.to_string(), status.status);
          self.mirror_pass().await;
      }

      /// Kubernetes mode's pass: no planner, the fleet phase recomputed from
      /// the mirrored agents, persisted and published.
      async fn mirror_pass(&mut self) {
          let terminating = matches!(self.record.desired, Desired::Down { .. });
          finish_pass(&mut self.record.status, terminating, true);
          self.last_pass_clean = true;
          self.persist().await;
          self.publish();
      }
  ```
- At the top of `pass()`, before the system-pool check:
  ```rust
          if self.ports.kube.is_some() {
              return self.mirror_pass().await;
          }
  ```

`crates/balerix-server/src/daemon.rs`, next to `runner()`:

```rust
    /// Spec O §7: the link hub, when this daemon is in Kubernetes mode.
    pub fn kube(&self) -> Option<&Arc<crate::kube::LinkHub>> {
        self.ports.kube.as_ref()
    }

    /// A sidecar's `status` frame, to its fleet's actor. An unknown fleet
    /// (the operator deleted it while the pod lived) is dropped.
    pub async fn link_status(&self, agent: &AgentId, status: balerix_api::LinkStatus) {
        if let Some(h) = self.fleets.read().await.get(&agent.fleet) {
            let _ = h
                .tx
                .send(Msg::LinkStatus {
                    agent: agent.clone(),
                    status,
                })
                .await;
        }
    }

    pub async fn link_down(&self, agent: &AgentId) {
        if let Some(h) = self.fleets.read().await.get(&agent.fleet) {
            let _ = h
                .tx
                .send(Msg::LinkDown {
                    agent: agent.clone(),
                })
                .await;
        }
    }
```

`crates/balerix-server/src/api.rs`, in `router`, the `agents` group:

```rust
    let agents = Router::new()
        .route(
            "/v1/agents/{fleet}/{crew}/{agent}/events",
            post(hooks::events),
        )
        .route(
            "/v1/agents/{fleet}/{crew}/{agent}/link",
            get(crate::kube::link::link),
        )
        .layer(DefaultBodyLimit::max(1 << 20));
```

`crates/balerix-server/src/testing.rs`: add `pub hub: Option<Arc<LinkHub>>` to `Harness` (set `None` in `new`/`with_policy`) and:

```rust
    /// Kubernetes mode (Spec O §7): the runner and the workspace reader are
    /// the link hub and `Ports.kube` is set, so actors mirror and plan
    /// nothing. The fake materializer stays: nothing calls it.
    pub fn kube(resync: Duration) -> Self {
        let mut h = Self::new(resync);
        let hub = LinkHub::new();
        let ports = Ports {
            runner: hub.clone(),
            workspace: hub.clone(),
            kube: Some(hub.clone()),
            ..Ports::clone_from_arc(&h.ports)
        };
        h.ports = Arc::new(ports);
        h.hub = Some(hub);
        h
    }
```
`Ports` is not `Clone` (it holds `Arc`s and plain values; it can derive nothing because of `dyn` fields but every field is `Clone`): add `#[derive(Clone)]` to `Ports` in `actor.rs` and write the literal as `..(*h.ports).clone()`. Every `Ports { … }` literal gets `kube: None`: `testing.rs` (`new`/`with_policy`) and `crates/balerix/src/commands/serve.rs:162-173`.

- [ ] **Step 6: Run the test, then the server and binary suites**

Run: `mise x -- cargo test -p balerix-server --test kube_link_it` then `mise x -- cargo test -p balerix-server` and `mise x -- cargo test -p balerix`
Expected: 2 passed; everything else unchanged (`Ports.kube` is `None` everywhere else; the agents group gained a route no existing test hits).

- [ ] **Step 7: Gate and commit**

Run: `mise run check`

```bash
git add crates/balerix-core/src/ports.rs crates/balerix-server crates/balerix/src/commands/serve.rs
git commit -m "feat(server): the sidecar link hub: AgentRunner and WorkspaceReader over one WebSocket per agent (Spec O §7.2)"
```

---

### Task 7: `balerix-server`: Kubernetes mode of the daemon

**Files:**
- Modify: `crates/balerix-server/src/daemon.rs:94-112, 506-524, 527-547, 561-760, 804-830, 966-1004`
- Modify: `crates/balerix-server/src/actor.rs:33-57, 189-236, 283-312` (tokens on `Apply`; `SetStopped` and `Down` in Kubernetes mode; the stopped-set reconciliation on `LinkStatus`)
- Modify: `crates/balerix-server/src/api.rs:276-302, 171-181` (`agent_tokens` dispatch, `/readyz`)
- Create: `crates/balerix-server/src/kube/idle.rs`
- Modify: `crates/balerix-server/src/kube/mod.rs`, `src/lib.rs`
- Create: `crates/balerix-server/tests/kube_api_it.rs`

**Interfaces:**
- Consumes: `LinkHub::call`, `Ports.kube`, `Msg::LinkStatus` (Task 6); `FleetRequest.agent_tokens`, `AgentTokens` (Task 2); `check_owner`, `apply_as`, `down_as`, `set_stopped`, `execute_action` (existing); `SystemPoolState`, `Shared.system_pool` (existing).
- Produces, for Tasks 9 and 13:
  - `Caller::Kubernetes`; `Daemon::apply_kube(&self, name: &FleetName, spec: FleetSpec, agent_tokens: AgentTokens) -> Result<FleetRecord, DaemonError>`; `Daemon::system_pool_state(&self) -> SystemPoolState`.
  - `apply_as(…, agent_tokens: AgentTokens)` and `Msg::Apply { …, agent_tokens: AgentTokens }`.
  - `PUT /v1/fleets/{name}` with `agent_tokens`: 400 `agent_tokens is accepted only by a daemon in kubernetes mode` in tmux mode; otherwise an upsert recorded with `owner: "kubernetes"`, the tokens becoming the agents' hook secrets, every wanted agent present in `status.agents` as `Pending`. 400 `agent_tokens: no token for <id>` / `agent_tokens.<id>: a token is at least 32 characters` / `agent_tokens: <id> is not an agent of the fleet`. `POST /v1/fleets` with `agent_tokens`: 400 `agent_tokens: use PUT /v1/fleets/{name}`.
  - On a fleet owned by `kubernetes`: admin `PUT` without tokens, `POST`, and `DELETE` without `force` answer 409 `fleet <name> is managed by kubernetes; change it through its Fleet object`; `DELETE …?force=true` downs it.
  - `GET /readyz`: 200 `ready` when the pool channel is `Ready`; 503 `{"error":"daemon pool: pending"}` / `{"error":"daemon pool: <reason>"}` otherwise.
  - `balerix_server::kube::{NoFiles, NoPool}`: `NoFiles` implements `Materializer` with every method `Err(MaterializeError::Invalid { id, message: "a daemon in kubernetes mode materialises nothing; the sidecar does (Spec O §7.2)" })`; `NoPool` implements `SystemToolchain` with `ensure_system_pool` → `Ok(())`.
  - Actor in Kubernetes mode: `SetStopped` sends `stop`/`restart` over the link (warns on failure, records the set regardless); `Down` sends `stop` to every wanted agent, clears `status.agents`, becomes `Down`; a `LinkStatus` whose phase disagrees with the stopped set (and the fleet is `Up`) gets a `stop` or `restart`.

- [ ] **Step 1: Write the failing integration test**

Create `crates/balerix-server/tests/kube_api_it.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §7.3: the operator's apply, the owner rule, readiness, and the
//! stopped set over the link.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use balerix_api::{
    AgentPhase, AgentSettings, AgentStatus, CrewSpec, FleetPhase, FleetSpec, Keep,
    LINK_PROTOCOL_HEADER, LinkOp, LinkReply, LinkRequest, LinkResult, LinkStatus, PluginAction,
    SidecarFrame,
};
use balerix_core::{AgentId, PassThrough};
use balerix_server::testing::Harness;
use balerix_server::{Daemon, router, serve};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn spec() -> FleetSpec {
    FleetSpec {
        name: "f".into(),
        tools: BTreeMap::new(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/api".into(),
                git_ref: "main".into(),
                agents: BTreeMap::from([("a".to_string(), AgentSettings::default())]),
                ..CrewSpec::default()
            },
        )]),
    }
}

struct World {
    daemon: Arc<Daemon>,
    base: String,
    port: u16,
    _dir: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

impl World {
    /// A blocking request off the runtime, as `api_it` makes them.
    async fn call(&self, method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> (u16, Value) {
        let url = format!("{}{path}", self.base);
        let token = token.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(5)))
                .http_status_as_error(false)
                .build()
                .into();
            let mut req = match method {
                "GET" => agent.get(&url).force_send_body(),
                "POST" => agent.post(&url),
                "PUT" => agent.put(&url),
                "DELETE" => agent.delete(&url).force_send_body(),
                _ => unreachable!(),
            };
            if let Some(t) = token {
                req = req.header("Authorization", &format!("Bearer {t}"));
            }
            let mut resp = match body {
                Some(b) => req.send_json(&b).unwrap(),
                None => req.send_empty().unwrap(),
            };
            let status = resp.status().as_u16();
            let text = resp.body_mut().read_to_string().unwrap();
            (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
        })
        .await
        .unwrap()
    }
}

async fn world(h: &Harness) -> World {
    let dir = tempfile::tempdir().unwrap();
    let daemon = h.daemon(Arc::new(PassThrough), dir.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve(listener, router(daemon.clone()), async {
        let _ = rx.await;
    }));
    World {
        daemon,
        base: format!("http://127.0.0.1:{port}"),
        port,
        _dir: dir,
        stop: Some(stop),
    }
}

fn request(tokens: Option<Value>) -> Value {
    let mut v = json!({ "spec": serde_json::to_value(spec()).unwrap() });
    if let Some(t) = tokens {
        v["agent_tokens"] = t;
    }
    v
}

async fn wait_for(mut f: impl AsyncFnMut() -> bool) {
    let start = Instant::now();
    while !f().await {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tmux_daemon_refuses_agent_tokens() {
    let h = Harness::new(Duration::from_secs(3600));
    let w = world(&h).await;
    let (status, body) = w
        .call("PUT", "/v1/fleets/f", Some("admin-tok"), Some(request(Some(json!({ "f/c/a": TOKEN })))))
        .await;
    assert_eq!(
        (status, body["error"].as_str().unwrap()),
        (400, "agent_tokens is accepted only by a daemon in kubernetes mode")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_operators_put_records_a_kubernetes_fleet_the_cli_cannot_touch() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world(&h).await;
    let id: AgentId = "f/c/a".parse().unwrap();

    let (status, body) = w
        .call("PUT", "/v1/fleets/f", Some("admin-tok"), Some(request(Some(json!({ "f/c/a": TOKEN })))))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["owner"], "kubernetes");
    assert_eq!(body["status"]["agents"]["f/c/a"]["phase"], "pending");
    assert_eq!(w.daemon.hook_secret(&id).await.as_deref(), Some(TOKEN));

    // the CLI's shapes answer 409
    let managed = "fleet f is managed by kubernetes; change it through its Fleet object";
    let (status, body) = w
        .call("PUT", "/v1/fleets/f", Some("admin-tok"), Some(request(None)))
        .await;
    assert_eq!((status, body["error"].as_str().unwrap()), (409, managed));
    let (status, body) = w
        .call("POST", "/v1/fleets", Some("admin-tok"), Some(request(None)))
        .await;
    assert_eq!((status, body["error"].as_str().unwrap()), (409, managed));
    let (status, body) = w
        .call("DELETE", "/v1/fleets/f?keep_repos=false&keep_sessions=false&purge=false&force=false", Some("admin-tok"), None)
        .await;
    assert_eq!((status, body["error"].as_str().unwrap()), (409, managed));

    // a second operator PUT is an upsert that keeps the token
    let (status, _) = w
        .call("PUT", "/v1/fleets/f", Some("admin-tok"), Some(request(Some(json!({ "f/c/a": TOKEN })))))
        .await;
    assert_eq!(status, 200);
    assert_eq!(w.daemon.hook_secret(&id).await.as_deref(), Some(TOKEN));

    // the token rules
    let (status, body) = w
        .call("PUT", "/v1/fleets/f", Some("admin-tok"), Some(request(Some(json!({})))))
        .await;
    assert_eq!((status, body["error"].as_str().unwrap()), (400, "agent_tokens: no token for f/c/a"));
    let (status, body) = w
        .call("PUT", "/v1/fleets/f", Some("admin-tok"), Some(request(Some(json!({ "f/c/a": "short" })))))
        .await;
    assert_eq!(
        (status, body["error"].as_str().unwrap()),
        (400, "agent_tokens.f/c/a: a token is at least 32 characters")
    );
    let (status, body) = w
        .call("PUT", "/v1/fleets/f", Some("admin-tok"), Some(request(Some(json!({ "f/c/a": TOKEN, "f/c/zed": TOKEN })))))
        .await;
    assert_eq!(
        (status, body["error"].as_str().unwrap()),
        (400, "agent_tokens: f/c/zed is not an agent of the fleet")
    );
    let (status, body) = w
        .call("POST", "/v1/fleets", Some("admin-tok"), Some(request(Some(json!({ "f/c/a": TOKEN })))))
        .await;
    assert_eq!(
        (status, body["error"].as_str().unwrap()),
        (400, "agent_tokens: use PUT /v1/fleets/{name}")
    );

    // the operator's down is a forced one
    let (status, body) = w
        .call("DELETE", "/v1/fleets/f?keep_repos=false&keep_sessions=false&purge=false&force=true", Some("admin-tok"), None)
        .await;
    assert_eq!(status, 200, "{body}");
    wait_for(async || w.daemon.get(&"f".parse().unwrap()).await.unwrap().is_down()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readyz_follows_the_pool_channel() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world(&h).await;
    wait_for(async || w.call("GET", "/readyz", None, None).await.0 == 200).await;
    let (status, body) = w.call("GET", "/readyz", None, None).await;
    assert_eq!((status, body), (200, Value::String("ready".into())));
}

async fn link(port: u16, token: &str) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let mut req = format!("ws://127.0.0.1:{port}/v1/agents/f/c/a/link")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    req.headers_mut().insert(LINK_PROTOCOL_HEADER, "1".parse().unwrap());
    tokio_tungstenite::connect_async(req).await.unwrap().0
}

fn status_frame(phase: AgentPhase) -> Message {
    Message::Text(
        serde_json::to_string(&SidecarFrame::Status(LinkStatus {
            status: AgentStatus {
                phase,
                ..AgentStatus::default()
            },
            pid: Some(56),
            hook_failures: 0,
        }))
        .unwrap()
        .into(),
    )
}

async fn next_request(ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>) -> LinkRequest {
    loop {
        match ws.next().await.unwrap().unwrap() {
            Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
            Message::Ping(_) => {}
            other => panic!("{other:?}"),
        }
    }
}

async fn reply_ok(ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, id: u64) {
    ws.send(Message::Text(
        serde_json::to_string(&SidecarFrame::Reply(LinkReply {
            id,
            result: LinkResult::Ok,
        }))
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
}

/// Review Focus 5 and §16.4 over the link: `stop`/`restart` travel as
/// frames, and a status frame that disagrees with the stopped set is
/// corrected, which is how a sidecar that was away learns of a stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_stopped_set_travels_over_the_link_and_is_reconciled_on_reconnect() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world(&h).await;
    let name = "f".parse().unwrap();
    let id: AgentId = "f/c/a".parse().unwrap();
    w.daemon
        .apply_kube(&name, spec(), BTreeMap::from([("f/c/a".to_string(), TOKEN.to_string())]))
        .await
        .unwrap();
    let mut ws = link(w.port, TOKEN).await;
    ws.send(status_frame(AgentPhase::Ready)).await.unwrap();
    wait_for(async || w.daemon.get(&name).await.unwrap().status.agents["f/c/a"].phase == AgentPhase::Ready).await;

    // a stop action: the frame goes out, the set is recorded
    let (d, i) = (w.daemon.clone(), id.clone());
    let action = tokio::spawn(async move { d.execute_action(&i, &PluginAction::Stop, None).await });
    let req = next_request(&mut ws).await;
    assert_eq!(req.op, LinkOp::Stop);
    reply_ok(&mut ws, req.id).await;
    action.await.unwrap().unwrap();
    assert!(w.daemon.get(&name).await.unwrap().stopped.contains("f/c/a"));
    // the sidecar reports Stopped: nothing more is sent
    ws.send(status_frame(AgentPhase::Stopped)).await.unwrap();
    wait_for(async || w.daemon.get(&name).await.unwrap().status.agents["f/c/a"].phase == AgentPhase::Stopped).await;

    // the link drops; a restart action lands with nobody listening
    ws.close(None).await.unwrap();
    wait_for(async || !w.daemon.kube().unwrap().linked(&id)).await;
    w.daemon
        .execute_action(&id, &PluginAction::Restart, None)
        .await
        .unwrap();
    assert!(!w.daemon.get(&name).await.unwrap().stopped.contains("f/c/a"));

    // the sidecar comes back still stopped: it is told to restart
    let mut ws = link(w.port, TOKEN).await;
    ws.send(status_frame(AgentPhase::Stopped)).await.unwrap();
    let req = next_request(&mut ws).await;
    assert_eq!(req.op, LinkOp::Restart);
    reply_ok(&mut ws, req.id).await;
    ws.send(status_frame(AgentPhase::Ready)).await.unwrap();

    // and the other way: a stop recorded while away
    ws.close(None).await.unwrap();
    wait_for(async || !w.daemon.kube().unwrap().linked(&id)).await;
    w.daemon
        .execute_action(&id, &PluginAction::Stop, None)
        .await
        .unwrap();
    let mut ws = link(w.port, TOKEN).await;
    ws.send(status_frame(AgentPhase::Ready)).await.unwrap();
    let req = next_request(&mut ws).await;
    assert_eq!(req.op, LinkOp::Stop);
    reply_ok(&mut ws, req.id).await;

    // down: a stop to every linked agent, then the fleet is Down and
    // nothing is sent for the Stopped report that follows
    let (d, n) = (w.daemon.clone(), name.clone());
    let down = tokio::spawn(async move { d.down_as(&n, Keep::default(), false, &balerix_server::Caller::Admin { force: true }).await });
    let req = next_request(&mut ws).await;
    assert_eq!(req.op, LinkOp::Stop);
    reply_ok(&mut ws, req.id).await;
    let record = down.await.unwrap().unwrap();
    assert_eq!(record.status.phase, FleetPhase::Down);
    assert!(record.is_down());
    ws.send(status_frame(AgentPhase::Stopped)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    // no request followed: the next frame the fake sees is a ping or nothing
    let record = w.daemon.get(&name).await.unwrap();
    assert!(record.is_down());
}
```

- [ ] **Step 2: Run to see it fail**

Run: `mise x -- cargo test -p balerix-server --test kube_api_it`
Expected: compile errors (`apply_kube`, `Caller::Kubernetes` do not exist); after stubbing, the 400s and 409s are not answered.

- [ ] **Step 3: The caller, the owner rule, the apply**

`crates/balerix-server/src/daemon.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// The admin API. `force` lets `down` take a managed fleet
    /// (`balerix down --force`, and the operator's own `DELETE`); nothing
    /// lets the admin apply one.
    Admin { force: bool },
    /// A plugin with `manage`, by name.
    Plugin(AgentName),
    /// The operator (Spec O §7.3): a `PUT` carrying `agent_tokens`.
    Kubernetes,
}

impl Caller {
    fn owner(&self) -> Option<String> {
        match self {
            Caller::Admin { .. } => None,
            Caller::Plugin(p) => Some(p.to_string()),
            Caller::Kubernetes => Some(crate::kube::KUBERNETES_OWNER.to_string()),
        }
    }
}
```

`check_owner`:

```rust
    fn check_owner(
        name: &FleetName,
        owner: Option<&str>,
        caller: &Caller,
        downing: bool,
    ) -> Result<(), DaemonError> {
        let managed = |p: &str| {
            DaemonError::Managed(if p == crate::kube::KUBERNETES_OWNER {
                format!("fleet {name} is managed by kubernetes; change it through its Fleet object")
            } else {
                format!("fleet {name} is managed by plugin {p}")
            })
        };
        match (caller, owner) {
            (Caller::Admin { force }, Some(p)) if !(downing && *force) => Err(managed(p)),
            (Caller::Plugin(me), Some(p)) if p != me.as_str() => Err(managed(p)),
            (Caller::Plugin(_), None) => Err(DaemonError::Managed(format!(
                "fleet {name} is not managed by a plugin"
            ))),
            (Caller::Kubernetes, Some(p)) if p != crate::kube::KUBERNETES_OWNER => Err(managed(p)),
            _ => Ok(()),
        }
    }
```

`apply_as` gains a last parameter `agent_tokens: balerix_api::AgentTokens` and passes it in `Msg::Apply { spec, credentials, agent_tokens, reply }`. `apply` (the admin's) and `manage_fleet` pass `BTreeMap::new()`; so does every other call site (`grep -rn "apply_as(" crates/balerix-server`: three in `src/`, plus the unit tests at the bottom of `daemon.rs` and any in `tests/`). `Msg::Apply` literals in `testing.rs` and the tests gain `agent_tokens: BTreeMap::new()`. Add, after `apply`:

```rust
    /// Spec O §7.3: the operator's apply. A resolved spec plus one token
    /// per agent, which becomes the agent's hook secret (its sidecar
    /// presents it on the hook route and on the link). An upsert: the
    /// operator re-sends on every reconcile. The tokens are checked
    /// against the spec before the owner rule or any plugin is consulted.
    pub async fn apply_kube(
        &self,
        name: &FleetName,
        spec: FleetSpec,
        agent_tokens: balerix_api::AgentTokens,
    ) -> Result<FleetRecord, DaemonError> {
        if self.ports.kube.is_none() {
            return Err(DaemonError::Invalid(
                "agent_tokens is accepted only by a daemon in kubernetes mode".into(),
            ));
        }
        let fleet = Fleet::try_from(spec.clone()).map_err(|e| DaemonError::Invalid(e.to_string()))?;
        let wanted: Vec<String> = ResolvedAgent::from_fleet(&fleet)
            .into_iter()
            .map(|a| a.id.to_string())
            .collect();
        for key in &wanted {
            match agent_tokens.get(key) {
                None => {
                    return Err(DaemonError::Invalid(format!(
                        "agent_tokens: no token for {key}"
                    )));
                }
                Some(t) if t.len() < 32 => {
                    return Err(DaemonError::Invalid(format!(
                        "agent_tokens.{key}: a token is at least 32 characters"
                    )));
                }
                Some(_) => {}
            }
        }
        for key in agent_tokens.keys() {
            if !wanted.contains(key) {
                return Err(DaemonError::Invalid(format!(
                    "agent_tokens: {key} is not an agent of the fleet"
                )));
            }
        }
        self.apply_as(
            name,
            spec,
            CredentialBundle::default(),
            ApplyMode::Upsert,
            &Caller::Kubernetes,
            agent_tokens,
        )
        .await
    }

    /// Spec F's channel, for `/readyz`.
    pub fn system_pool_state(&self) -> SystemPoolState {
        self.shared.system_pool.borrow().clone()
    }
```

(`ResolvedAgent` and `Fleet` are already imported in `daemon.rs` for `wanted_agents`-like code; add `use balerix_core::ResolvedAgent;` if not.)

- [ ] **Step 4: The actor: tokens, the stopped set over the link, down**

`crates/balerix-server/src/actor.rs`:

`Msg::Apply` gains `agent_tokens: balerix_api::AgentTokens`. `Actor::apply(&mut self, spec, credentials, agent_tokens)`: the secret for each wanted id is

```rust
            let secret = agent_tokens
                .get(&key)
                .cloned()
                .or_else(|| self.secrets.hook_secrets.get(&key).cloned())
                .unwrap_or_else(|| random_hex(32));
```

and, after `self.secrets.hook_secrets = next;`, in Kubernetes mode make every wanted agent visible before its sidecar links:

```rust
        if self.ports.kube.is_some() {
            for id in &wanted {
                self.record.status.entry(&id.to_string());
            }
        }
```

A helper for the frames:

```rust
    /// One frame to one agent's sidecar, off the runtime (the hub's call
    /// blocks). A failure is logged: the set is the record's, and the
    /// next status frame reconciles it (`link_status`).
    async fn link_op(&self, agent: &AgentId, op: balerix_api::LinkOp) {
        let Some(hub) = self.ports.kube.clone() else { return };
        let id = agent.clone();
        let what = format!("{op:?}");
        match tokio::task::spawn_blocking(move || hub.call(&id, op)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!(agent = %agent, "link {what} failed: {e}"),
            Err(e) => tracing::error!(agent = %agent, "link task panicked: {e}"),
        }
    }
```

The `SetStopped` arm:

```rust
                Some(Msg::SetStopped { agent, stopped, reply }) => {
                    let key = agent.to_string();
                    if stopped {
                        self.record.stopped.insert(key);
                    } else {
                        self.record.stopped.remove(&key);
                    }
                    self.persist().await;
                    self.publish();
                    if self.ports.kube.is_some() {
                        let op = if stopped { balerix_api::LinkOp::Stop } else { balerix_api::LinkOp::Restart };
                        self.link_op(&agent, op).await;
                    }
                    self.pass().await;
                    let _ = reply.send(self.record.clone());
                }
```

The `Down` arm, in Kubernetes mode, before `self.pass().await`:

```rust
                    if self.ports.kube.is_some() {
                        for id in self.wanted_agents() {
                            self.link_op(&id, balerix_api::LinkOp::Stop).await;
                        }
                        // the pods are the operator's to remove; the
                        // Daemon's view of them ends with the down
                        self.record.status.agents.clear();
                    }
```

`link_status` becomes:

```rust
    /// Spec O §7.2: the sidecar's status is the Daemon's observed state.
    /// A frame that disagrees with the stopped set (a `Ready` agent the
    /// set holds, a `Stopped` one it does not) gets the frame it missed,
    /// which is how a sidecar that was away during `stop` or `restart`
    /// learns of it. A downed fleet corrects nothing.
    async fn link_status(&mut self, agent: AgentId, status: balerix_api::LinkStatus) {
        let key = agent.to_string();
        let wanted_stopped = self.record.stopped.contains(&key);
        let reported_stopped = status.status.phase == AgentPhase::Stopped;
        self.record.status.agents.insert(key, status.status);
        if self.record.desired == Desired::Up && wanted_stopped != reported_stopped {
            let op = if wanted_stopped { balerix_api::LinkOp::Stop } else { balerix_api::LinkOp::Restart };
            self.link_op(&agent, op).await;
        }
        self.mirror_pass().await;
    }
```

(`AgentPhase` from `balerix_api`; `Desired` derives `PartialEq`.)

- [ ] **Step 5: The routes**

`crates/balerix-server/src/api.rs`:

```rust
async fn create_fleet(
    State(state): State<AppState>,
    b: Result<Json<FleetRequest>, JsonRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    let req = body(b)?;
    if req.agent_tokens.is_some() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "agent_tokens: use PUT /v1/fleets/{name}",
        ));
    }
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
    name: Result<Path<String>, PathRejection>,
    b: Result<Json<FleetRequest>, JsonRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    let req = body(b)?;
    let name = fleet_name(&path_name(name)?)?;
    let record = match req.agent_tokens {
        // Spec O §7.3: the operator's apply
        Some(tokens) => state.daemon.apply_kube(&name, req.spec, tokens).await?,
        None => {
            state
                .daemon
                .apply(&name, req.spec, req.credentials, true)
                .await?
        }
    };
    Ok(Json(record))
}

/// `GET /readyz` (Spec O §7.3): Spec F's pool channel and nothing else.
async fn readyz(State(state): State<AppState>) -> Response {
    match state.daemon.system_pool_state() {
        SystemPoolState::Ready => "ready".into_response(),
        SystemPoolState::Pending => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "daemon pool: pending").into_response()
        }
        SystemPoolState::Unready { reason } => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("daemon pool: {reason}"),
        )
        .into_response(),
    }
}
```

and in `router`: `.route("/readyz", get(readyz))` next to `/healthz`. Import `crate::system_pool::SystemPoolState`.

- [ ] **Step 6: The idle ports**

Create `crates/balerix-server/src/kube/idle.rs`:

```rust
//! The ports a Kubernetes-mode daemon has no work for (Spec O §7.2): the
//! sidecar materialises, and the shared volume's daemon pool is a Job's
//! (§8.3). `NoFiles` refuses, since nothing should call it; `NoPool`
//! answers ready, so `/readyz` and the actors' gate open.

use balerix_api::CredentialBundle;
use balerix_core::{
    AgentId, AgentName, CrewRef, CrewTools, HookTarget, Keep, LaunchPlan, MaterializeError,
    Materializer, RepoRef, ResolvedAgent, ResolvedPlugin, SystemToolchain,
};

pub struct NoFiles;

fn refused(id: String) -> MaterializeError {
    MaterializeError::Invalid {
        id,
        message: "a daemon in kubernetes mode materialises nothing; the sidecar does (Spec O §7.2)"
            .into(),
    }
}

impl Materializer for NoFiles {
    fn ensure_crew(
        &self,
        crew: &CrewRef,
        _repo: &RepoRef,
        _git_ref: &str,
        _git: &balerix_api::GitSettings,
        _creds: &CredentialBundle,
        _tools: CrewTools<'_>,
    ) -> Result<(), MaterializeError> {
        Err(refused(crew.to_string()))
    }
    fn materialize(
        &self,
        agent: &ResolvedAgent,
        _creds: &CredentialBundle,
        _hooks: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        Err(refused(agent.id.to_string()))
    }
    fn remove_agent(&self, agent: &AgentId) -> Result<(), MaterializeError> {
        Err(refused(agent.to_string()))
    }
    fn remove_crew(&self, crew: &CrewRef, _keep: Keep) -> Result<(), MaterializeError> {
        Err(refused(crew.to_string()))
    }
    fn materialize_plugin(
        &self,
        plugin: &ResolvedPlugin,
        _host: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        Err(refused(plugin.name.to_string()))
    }
    fn purge_plugin(&self, name: &AgentName) -> Result<(), MaterializeError> {
        Err(refused(name.to_string()))
    }
}

pub struct NoPool;

impl SystemToolchain for NoPool {
    fn ensure_system_pool(&self) -> Result<(), MaterializeError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_files_refuses_and_no_pool_is_ready() {
        let id: AgentId = "f/c/a".parse().unwrap();
        let err = NoFiles.remove_agent(&id).unwrap_err();
        assert_eq!(
            err.to_string(),
            "f/c/a: a daemon in kubernetes mode materialises nothing; the sidecar does (Spec O §7.2)"
        );
        assert!(NoPool.ensure_system_pool().is_ok());
    }
}
```

(`ResolvedPlugin.name` is the field the plugin's id comes from; check `balerix-core/src/plugin.rs` and use the field that holds its `AgentName`.) The `Display` of `MaterializeError::Invalid` begins with the id (the crate convention, AGENTS.md); adjust the assertion to the exact rendering `cargo test` prints if the format differs. Add `pub mod idle;` and `pub use idle::{NoFiles, NoPool};` to `kube/mod.rs`, and `NoFiles, NoPool` to the `pub use kube::{…}` in `lib.rs`.

- [ ] **Step 7: Run the tests, then everything**

Run: `mise x -- cargo test -p balerix-server --test kube_api_it` then `mise x -- cargo test -p balerix-server` and `mise x -- cargo test -p balerix`
Expected: 4 passed; `api_it`'s existing 409 texts (`is managed by plugin …`) unchanged.

- [ ] **Step 8: Gate and commit**

Run: `mise run check`

```bash
git add crates/balerix-server
git commit -m "feat(server): Kubernetes mode: the operator's PUT with agent_tokens, the mirror actor, stop and restart over the link, /readyz (Spec O §7.3)"
```

---

### Task 8: `balerix-server`: attach over the link

**Files:**
- Create: `crates/balerix-server/src/kube/pty.rs`
- Modify: `crates/balerix-server/src/kube/link.rs` (`attach_waiting`, `attach`, `attach_arrived`, the `link_attach` route handler)
- Modify: `crates/balerix-server/src/kube/mod.rs`, `src/api.rs:153-160`
- Create: `crates/balerix-server/tests/kube_attach_it.rs`

**Interfaces:**
- Consumes: `LinkHub::call`, `LinkOp::Attach { session }` (Tasks 2, 6); `PtyStream` (core); `ResizeFrame` (api); `random_hex` (`crate::vault`).
- Produces, for the sidecar (Task 12):
  - Route `GET /v1/agents/{fleet}/{crew}/{agent}/link/attach/{session}` (WebSocket), authenticated like the link route; 1008 `unknown attach session` if nothing waits for `session`.
  - The protocol on that socket is the plugin attach protocol reversed: binary frames are terminal bytes both ways; the Daemon sends `{"resize":{"cols":N,"rows":N}}` text frames; close 1000 when the plugin's side is done.
  - `LinkHub::attach(&self, agent) -> Result<Box<dyn PtyStream>, RunnerError>`: sends `Attach { session }`, waits at most `ATTACH_WAIT` (5 s) for the second socket; `RunnerError::Link { message: "the sidecar did not open the attach socket" }` otherwise.

- [ ] **Step 1: Write the failing integration test**

Create `crates/balerix-server/tests/kube_attach_it.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §7.2: `attach` over the link. The Daemon asks for a session, the
//! sidecar opens a second socket for it, and the plugin-facing `PtyStream`
//! is that socket.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use balerix_api::{
    AgentSettings, CrewSpec, FleetSpec, LINK_PROTOCOL_HEADER, LinkOp, LinkReply, LinkRequest,
    LinkResult, SidecarFrame,
};
use balerix_core::{AgentId, AgentRunner, PassThrough, RunnerError};
use balerix_server::testing::Harness;
use balerix_server::{Daemon, router, serve};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn spec() -> FleetSpec {
    FleetSpec {
        name: "f".into(),
        tools: BTreeMap::new(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/api".into(),
                git_ref: "main".into(),
                agents: BTreeMap::from([("a".to_string(), AgentSettings::default())]),
                ..CrewSpec::default()
            },
        )]),
    }
}

async fn ws(port: u16, path: &str) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://127.0.0.1:{port}{path}").into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    req.headers_mut().insert(LINK_PROTOCOL_HEADER, "1".parse().unwrap());
    tokio_tungstenite::connect_async(req).await.map(|(w, _)| w)
}

async fn next_request(link: &mut Ws) -> LinkRequest {
    loop {
        match link.next().await.unwrap().unwrap() {
            Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
            Message::Ping(_) => {}
            other => panic!("{other:?}"),
        }
    }
}

struct World {
    daemon: Arc<Daemon>,
    port: u16,
    _dir: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}
impl Drop for World {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

async fn world() -> World {
    let h = Harness::kube(Duration::from_secs(3600));
    let dir = tempfile::tempdir().unwrap();
    let daemon = h.daemon(Arc::new(PassThrough), dir.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve(listener, router(daemon.clone()), async {
        let _ = rx.await;
    }));
    daemon
        .apply_kube(&"f".parse().unwrap(), spec(), BTreeMap::from([("f/c/a".to_string(), TOKEN.to_string())]))
        .await
        .unwrap();
    World {
        daemon,
        port,
        _dir: dir,
        stop: Some(stop),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_rides_a_second_socket_the_sidecar_opens() {
    let w = world().await;
    let id: AgentId = "f/c/a".parse().unwrap();
    let hub = w.daemon.kube().unwrap().clone();
    let mut link = ws(w.port, "/v1/agents/f/c/a/link").await.unwrap();
    let start = Instant::now();
    while !hub.linked(&id) {
        assert!(start.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let (hub2, id2) = (hub.clone(), id.clone());
    let attach = tokio::task::spawn_blocking(move || hub2.attach(&id2));
    let req = next_request(&mut link).await;
    let LinkOp::Attach { session } = &req.op else {
        panic!("{req:?}");
    };
    // the sidecar: open the second socket, then answer
    let mut pty_ws = ws(w.port, &format!("/v1/agents/f/c/a/link/attach/{session}")).await.unwrap();
    link.send(Message::Text(
        serde_json::to_string(&SidecarFrame::Reply(LinkReply {
            id: req.id,
            result: LinkResult::Ok,
        }))
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let stream = attach.await.unwrap().unwrap();

    // bytes the plugin writes reach the sidecar's socket as binary
    let mut writer = stream.writer().unwrap();
    tokio::task::spawn_blocking(move || {
        writer.write_all(b"ls\r").unwrap();
        writer.flush().unwrap();
    })
    .await
    .unwrap();
    assert_eq!(pty_ws.next().await.unwrap().unwrap(), Message::Binary(b"ls\r".to_vec().into()));

    // bytes from the sidecar reach the plugin's reader
    pty_ws.send(Message::Binary(b"total 0\r\n".to_vec().into())).await.unwrap();
    let mut reader = stream.reader().unwrap();
    let got = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 64];
        let n = reader.read(&mut buf).unwrap();
        buf[..n].to_vec()
    })
    .await
    .unwrap();
    assert_eq!(got, b"total 0\r\n");

    // a resize is the one text frame
    stream.resize(100, 30).unwrap();
    assert_eq!(
        pty_ws.next().await.unwrap().unwrap(),
        Message::Text(r#"{"resize":{"cols":100,"rows":30}}"#.into())
    );
    assert!(stream.writer().is_err(), "the writer is taken once");

    // dropping the stream closes the second socket
    tokio::task::spawn_blocking(move || drop(stream)).await.unwrap();
    let start = Instant::now();
    loop {
        match pty_ws.next().await {
            Some(Ok(Message::Close(_))) | None => break,
            Some(Ok(_)) => {}
            Some(Err(_)) => break,
        }
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    // a sidecar that never opens the socket: the attach fails within the bound
    let (hub2, id2) = (hub.clone(), id.clone());
    let attach = tokio::task::spawn_blocking(move || hub2.attach(&id2));
    let req = next_request(&mut link).await;
    assert!(matches!(req.op, LinkOp::Attach { .. }));
    link.send(Message::Text(
        serde_json::to_string(&SidecarFrame::Reply(LinkReply {
            id: req.id,
            result: LinkResult::Ok,
        }))
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let err = attach.await.unwrap().unwrap_err();
    assert_eq!(
        err,
        RunnerError::Link {
            id: "f/c/a".into(),
            message: "the sidecar did not open the attach socket".into()
        }
    );

    // an attach socket nobody asked for is closed 1008
    let mut stray = ws(w.port, "/v1/agents/f/c/a/link/attach/nope").await.unwrap();
    match stray.next().await.unwrap().unwrap() {
        Message::Close(Some(frame)) => assert_eq!(u16::from(frame.code), 1008),
        other => panic!("{other:?}"),
    }
}
```

- [ ] **Step 2: Run to see it fail**

Run: `mise x -- cargo test -p balerix-server --test kube_attach_it`
Expected: fails: `attach` returns the Task 6 error, and the attach route is 404.

- [ ] **Step 3: `WsPty` and the pump**

Create `crates/balerix-server/src/kube/pty.rs`:

```rust
//! A `PtyStream` whose terminal is a WebSocket the sidecar opened (Spec O
//! §7.2 `attach`). The plugin-facing `attach::bridge` reads and writes it
//! exactly as it does a tmux PTY; this file turns those blocking reads and
//! writes into frames on the sidecar's socket.

use std::io::{self, Read, Write};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender};

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use balerix_api::{Resize, ResizeFrame};
use balerix_core::PtyStream;
use tokio::sync::mpsc;

/// What the plugin's side sends toward the sidecar.
pub(crate) enum Outbound {
    Bytes(Vec<u8>),
    Resize(u16, u16),
}

pub struct WsPty {
    reader: Mutex<Option<Receiver<Vec<u8>>>>,
    writer: Mutex<Option<mpsc::UnboundedSender<Outbound>>>,
    control: mpsc::UnboundedSender<Outbound>,
}

impl WsPty {
    pub(crate) fn new(from_ws: Receiver<Vec<u8>>, to_ws: mpsc::UnboundedSender<Outbound>) -> Self {
        Self {
            reader: Mutex::new(Some(from_ws)),
            writer: Mutex::new(Some(to_ws.clone())),
            control: to_ws,
        }
    }
}

fn taken() -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, "already taken")
}

impl PtyStream for WsPty {
    fn reader(&self) -> io::Result<Box<dyn Read + Send>> {
        let rx = self
            .reader
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(taken)?;
        Ok(Box::new(ChanReader { rx, buf: Vec::new() }))
    }
    fn writer(&self) -> io::Result<Box<dyn Write + Send>> {
        let tx = self
            .writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(taken)?;
        Ok(Box::new(ChanWriter { tx }))
    }
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.control
            .send(Outbound::Resize(cols, rows))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the attach socket is gone"))
    }
}

struct ChanReader {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
}

impl Read for ChanReader {
    /// Blocks for the next frame; EOF once the pump is gone.
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.buf.is_empty() {
            match self.rx.recv() {
                Ok(bytes) => self.buf = bytes,
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len());
        out[..n].copy_from_slice(&self.buf[..n]);
        self.buf.drain(..n);
        Ok(n)
    }
}

struct ChanWriter {
    tx: mpsc::UnboundedSender<Outbound>,
}

impl Write for ChanWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.tx
            .send(Outbound::Bytes(bytes.to_vec()))
            .map(|()| bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the attach socket is gone"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Drives the sidecar's attach socket until the plugin's side drops the
/// `WsPty` (every `Outbound` sender gone) or the socket ends.
pub(crate) async fn pump(
    mut socket: WebSocket,
    to_pty: Sender<Vec<u8>>,
    mut from_pty: mpsc::UnboundedReceiver<Outbound>,
) {
    loop {
        tokio::select! {
            out = from_pty.recv() => match out {
                Some(Outbound::Bytes(b)) => {
                    if socket.send(Message::Binary(Bytes::from(b))).await.is_err() {
                        break;
                    }
                }
                Some(Outbound::Resize(cols, rows)) => {
                    let frame = ResizeFrame { resize: Resize { cols, rows } };
                    let Ok(json) = serde_json::to_string(&frame) else { break };
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                None => {
                    let _ = socket
                        .send(Message::Close(Some(CloseFrame {
                            code: crate::attach::CLOSE_NORMAL,
                            reason: "the viewer is done".into(),
                        })))
                        .await;
                    break;
                }
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    if to_pty.send(b.to_vec()).is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    // dropping `to_pty` ends the reader with EOF; the bridge then closes
    // the plugin's socket 1000
}
```

- [ ] **Step 4: The hub's attach and the second route**

In `crates/balerix-server/src/kube/link.rs`:

- `LinkHub` gains `attach_waiting: Mutex<HashMap<String, SyncSender<super::pty::WsPty>>>` (keep `#[derive(Default)]`).
- A constant `pub const ATTACH_WAIT: Duration = Duration::from_secs(5);`.
- Replace the `attach` body in `impl AgentRunner for LinkHub`:

```rust
    /// Spec O §7.2: a session name goes out as `Attach`; the sidecar opens
    /// `GET …/link/attach/{session}` and answers; `attach_arrived` hands the
    /// socket here as a `WsPty`. The sidecar may open the socket before
    /// it answers, so the waiter is registered before the request leaves.
    fn attach(&self, agent: &AgentId) -> Result<Box<dyn PtyStream>, RunnerError> {
        let session = crate::vault::random_hex(16);
        let (tx, rx) = sync_channel(1);
        lock(&self.attach_waiting).insert(session.clone(), tx);
        let forget = || {
            lock(&self.attach_waiting).remove(&session);
        };
        match self.call(agent, LinkOp::Attach { session: session.clone() }) {
            Ok(LinkResult::Ok) => {}
            Ok(_) => {
                forget();
                return Err(runner_err(agent, LinkError::Unexpected("attach")));
            }
            Err(e) => {
                forget();
                return Err(runner_err(agent, e));
            }
        }
        match rx.recv_timeout(ATTACH_WAIT) {
            Ok(pty) => Ok(Box::new(pty)),
            Err(_) => {
                forget();
                Err(RunnerError::Link {
                    id: agent.to_string(),
                    message: "the sidecar did not open the attach socket".into(),
                })
            }
        }
    }
```

- A method and a handler:

```rust
impl LinkHub {
    /// The second socket: paired with the waiting `attach`, then pumped
    /// until the viewer is done. A session nobody waits for is closed 1008.
    pub async fn attach_arrived(self: Arc<Self>, session: String, mut socket: WebSocket) {
        let Some(waiter) = lock(&self.attach_waiting).remove(&session) else {
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: 1008,
                    reason: "unknown attach session".into(),
                })))
                .await;
            return;
        };
        let (to_pty_tx, to_pty_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let (from_pty_tx, from_pty_rx) = mpsc::unbounded_channel::<super::pty::Outbound>();
        let pty = super::pty::WsPty::new(to_pty_rx, from_pty_tx);
        if waiter.try_send(pty).is_err() {
            // the blocking attach gave up (ATTACH_WAIT)
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: crate::attach::CLOSE_ERROR,
                    reason: "the attach timed out".into(),
                })))
                .await;
            return;
        }
        super::pty::pump(socket, to_pty_tx, from_pty_rx).await;
    }
}

/// `GET /v1/agents/{fleet}/{crew}/{agent}/link/attach/{session}`.
pub(crate) async fn link_attach(
    State(state): State<AppState>,
    path: Result<Path<(String, String, String, String)>, PathRejection>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(hub) = state.daemon.kube().cloned() else {
        return ApiError::new(StatusCode::NOT_FOUND, "not a daemon in kubernetes mode")
            .into_response();
    };
    let Path((fleet, crew, agent, session)) = match path {
        Ok(p) => p,
        Err(e) => return ApiError::new(e.status(), e.body_text()).into_response(),
    };
    let unauthorized =
        || ApiError::new(StatusCode::UNAUTHORIZED, "unknown agent or bad secret").into_response();
    let Ok(id) = format!("{fleet}/{crew}/{agent}").parse::<AgentId>() else {
        return unauthorized();
    };
    let Some(token) = bearer(&headers) else {
        return unauthorized();
    };
    if !state.daemon.verify_secret(&id, token).await {
        return unauthorized();
    }
    ws.on_upgrade(move |socket| hub.attach_arrived(session, socket))
}
```

Factor the shared authentication of `link` and `link_attach` into `async fn authenticate(state: &AppState, fleet: &str, crew: &str, agent: &str, headers: &HeaderMap) -> Result<(Arc<LinkHub>, AgentId), Response>` so the two handlers do not repeat it. Register the route in `api.rs`'s `agents` group: `.route("/v1/agents/{fleet}/{crew}/{agent}/link/attach/{session}", get(crate::kube::link::link_attach))`. Add `pub mod pty;` to `kube/mod.rs` and `pub use link::ATTACH_WAIT;`.

- [ ] **Step 5: Run the attach test and the server suite**

Run: `mise x -- cargo test -p balerix-server --test kube_attach_it` then `mise x -- cargo test -p balerix-server`
Expected: 1 passed; all green.

- [ ] **Step 6: Gate and commit**

Run: `mise run check`

```bash
git add crates/balerix-server
git commit -m "feat(server): attach over the link: a second socket the sidecar opens, bridged as the plugin's PtyStream (Spec O §7.2)"
```

---

### Task 9: `balerix serve --mode kubernetes`: TLS, the admin token file, no plugins

**Files:**
- Modify: `Cargo.toml:30-60` (workspace dependencies)
- Modify: `crates/balerix-server/Cargo.toml` (`axum-server`, `rustls`)
- Create: `crates/balerix-server/src/kube/tls.rs`
- Modify: `crates/balerix-server/src/kube/mod.rs`, `src/lib.rs`
- Modify: `crates/balerix/Cargo.toml` (dev: `rcgen`, `rustls`, `rustls-pki-types`)
- Modify: `crates/balerix/src/cli.rs:186-200`
- Modify: `crates/balerix/src/commands/serve.rs:72-100, 138-220`
- Modify: `crates/balerix/tests/cli_serve.rs`

**Interfaces:**
- Consumes: `LinkHub`, `NoFiles`, `NoPool`, `Ports.kube` (Tasks 6–7); `router`, `Daemon::start`, `write_endpoint`, `write_pid` (existing).
- Produces, for Task 13:
  - `balerix serve --mode kubernetes --tls-cert <pem> --tls-key <pem> --admin-token-file <file> [--bind <addr>]`: serves HTTPS on `--bind` (any address; default from `config.toml` or `127.0.0.1:7643`), the endpoint file holds `https://<addr>`, the admin token is the file's trimmed content (at least 32 characters), no `plugins.yaml` is read, `-d` is refused.
  - Exact refusals: `serve --mode kubernetes needs --tls-cert, --tls-key and --admin-token-file (Spec O §7.3)`; `--tls-cert, --tls-key and --admin-token-file are for --mode kubernetes`; `--detach is not available with --mode kubernetes; a pod runs the daemon in the foreground`; `<file>: the admin token is at least 32 characters`.
  - `balerix_server::kube::{serve_tls, TlsServer}`: `async fn serve_tls(addr: SocketAddr, cert: &Path, key: &Path, router: Router) -> io::Result<TlsServer>`; `TlsServer::local_addr(&self).await -> Option<SocketAddr>`; `TlsServer::shutdown(self).await -> io::Result<()>` (graceful, 5 s).

- [ ] **Step 1: Dependencies**

Root `Cargo.toml`, in `[workspace.dependencies]` after `portable-pty`:

```toml
# Spec O §7.3, §10.3: the Daemon serves TLS in Kubernetes mode. axum-server
# binds rustls to axum; `tls-rustls-no-provider` so the crypto provider is
# chosen here — ring, pure Rust, which builds for static musl without a C
# toolchain — rather than aws-lc-rs. No reqwest feature changes: the core
# `reqwest` keeps `json` alone (scripts/check-core-deps.sh).
axum-server = { version = "0.8.0", default-features = false, features = ["tls-rustls-no-provider"] }
rustls = { version = "0.23.45", default-features = false, features = ["ring", "std", "tls12", "logging"] }
rustls-pki-types = { version = "1.15.1", features = ["std"] }
# dev: a throwaway authority and a leaf for 127.0.0.1 in the TLS tests
rcgen = { version = "0.14.10", default-features = false, features = ["pem", "ring"] }
```

`crates/balerix-server/Cargo.toml` `[dependencies]`: `axum-server = { workspace = true }`, `rustls = { workspace = true }`. `crates/balerix/Cargo.toml` `[dev-dependencies]`: `rcgen = { workspace = true }`, `rustls = { workspace = true }`, `rustls-pki-types = { workspace = true }`.

Run `mise x -- cargo build --workspace --all-targets` and `scripts/check-core-deps.sh`. Expected: builds; the guard prints `core workspace clean: members match Spec H §3, reqwest features are 'json'`. Run `mise x -- cargo deny check licenses`: `ring` is `Apache-2.0 AND ISC`, both allowed; nothing new is needed in `deny.toml`.

- [ ] **Step 2: Write the failing CLI test**

Append to `crates/balerix/tests/cli_serve.rs`:

```rust
/// A throwaway authority and a leaf for 127.0.0.1, as PEM files in `dir`:
/// (ca.crt, tls.crt, tls.key).
fn tls_files(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(rcgen::DnType::CommonName, "balerix test authority");
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let ca_pem = dir.join("ca.crt");
    fs::write(&ca_pem, ca_cert.pem()).unwrap();
    let issuer = rcgen::Issuer::new(ca, ca_key);
    let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string(), "localhost".to_string()]).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = leaf.signed_by(&key, &issuer).unwrap();
    let cert_pem = dir.join("tls.crt");
    let key_pem = dir.join("tls.key");
    fs::write(&cert_pem, cert.pem()).unwrap();
    fs::write(&key_pem, key.serialize_pem()).unwrap();
    (ca_pem, cert_pem, key_pem)
}

/// One HTTPS GET over rustls on a plain TcpStream: the core workspace has
/// no TLS client (P3-1 holds for ureq and reqwest), and the test needs
/// none beyond rustls itself.
fn tls_get(addr: &str, ca: &Path, path: &str, token: Option<&str>) -> (u16, String) {
    use rustls_pki_types::pem::PemObject;
    use std::io::{Read, Write};
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pki_types::CertificateDer::pem_file_iter(ca).unwrap() {
        roots.add(cert.unwrap()).unwrap();
    }
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let name = rustls_pki_types::ServerName::try_from("127.0.0.1").unwrap();
    let mut conn = rustls::ClientConnection::new(std::sync::Arc::new(config), name).unwrap();
    let mut tcp = std::net::TcpStream::connect(addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut tls = rustls::Stream::new(&mut conn, &mut tcp);
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    tls.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{auth}\r\n")
            .as_bytes(),
    )
    .unwrap();
    let mut raw = Vec::new();
    let _ = tls.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status: u16 = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map_or(String::new(), |(_, b)| b.to_string());
    (status, body)
}

#[test]
fn kubernetes_mode_serves_tls_with_the_mounted_token_and_reads_no_plugins_file() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    let (ca, cert, key) = tls_files(home.path());
    let token_file = home.path().join("admin-token");
    fs::write(&token_file, "0123456789abcdef0123456789abcdef\n").unwrap();
    // a plugins.yaml a tmux daemon would read; kubernetes mode must not
    let config = home.path().join(".config/balerix");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("plugins.yaml"), "plugins:\n  - name: nope\n    source: ./nope\n").unwrap();
    let server_dir = home.path().join(".local/state/balerix/server");
    let child = balerix(home.path(), tools.path())
        .args(["serve", "--mode", "kubernetes", "--bind", "127.0.0.1:0", "--tmux-socket", "unused"])
        .arg("--tls-cert")
        .arg(&cert)
        .arg("--tls-key")
        .arg(&key)
        .arg("--admin-token-file")
        .arg(&token_file)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _kill = Kill(child);
    let url = wait_for_file(&server_dir.join("endpoint"));
    assert!(url.starts_with("https://127.0.0.1:"), "{url}");
    let addr = url.trim_start_matches("https://");
    assert_eq!(tls_get(addr, &ca, "/healthz", None), (200, "ok".into()));
    assert_eq!(tls_get(addr, &ca, "/readyz", None), (200, "ready".into()));
    assert_eq!(tls_get(addr, &ca, "/v1/fleets", None).0, 401);
    assert_eq!(
        tls_get(addr, &ca, "/v1/fleets", Some("0123456789abcdef0123456789abcdef")),
        (200, "[]".into())
    );
    assert!(
        !server_dir.join("token").exists(),
        "the admin token comes from the mounted file, none is generated"
    );
    // nothing under plugins/: the file was not read
    assert!(!home.path().join(".local/share/balerix/plugins").exists());
}

#[test]
fn kubernetes_mode_refuses_missing_tls_and_detach_and_tmux_mode_refuses_the_tls_flags() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    let out = balerix(home.path(), tools.path())
        .args(["serve", "--mode", "kubernetes", "--bind", "127.0.0.1:0"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains(
        "serve --mode kubernetes needs --tls-cert, --tls-key and --admin-token-file (Spec O §7.3)"
    ));
    let (_, cert, key) = tls_files(home.path());
    let token_file = home.path().join("admin-token");
    fs::write(&token_file, "0123456789abcdef0123456789abcdef\n").unwrap();
    let out = balerix(home.path(), tools.path())
        .args(["serve", "-d", "--mode", "kubernetes", "--bind", "127.0.0.1:0"])
        .arg("--tls-cert").arg(&cert).arg("--tls-key").arg(&key)
        .arg("--admin-token-file").arg(&token_file)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains(
        "--detach is not available with --mode kubernetes; a pod runs the daemon in the foreground"
    ));
    let out = balerix(home.path(), tools.path())
        .args(["serve", "--bind", "127.0.0.1:0"])
        .arg("--tls-cert").arg(&cert).arg("--tls-key").arg(&key)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains(
        "--tls-cert, --tls-key and --admin-token-file are for --mode kubernetes"
    ));
    fs::write(&token_file, "short\n").unwrap();
    let out = balerix(home.path(), tools.path())
        .args(["serve", "--mode", "kubernetes", "--bind", "127.0.0.1:0"])
        .arg("--tls-cert").arg(&cert).arg("--tls-key").arg(&key)
        .arg("--admin-token-file").arg(&token_file)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("the admin token is at least 32 characters"));
}
```

Add `use std::path::PathBuf;` to the file's imports.

- [ ] **Step 3: Run to see it fail**

Run: `mise x -- cargo test -p balerix --test cli_serve`
Expected: the two new tests fail (`--mode` is an unknown argument).

- [ ] **Step 4: `serve_tls`**

Create `crates/balerix-server/src/kube/tls.rs`:

```rust
//! TLS serving for Kubernetes mode (Spec O §7.3, §10.3): axum-server on
//! rustls with the ring provider, from a mounted certificate and key the
//! operator issued under its per-Daemon authority.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use axum::Router;
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;

pub struct TlsServer {
    handle: Handle<SocketAddr>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

/// Binds `addr` and serves `router` over TLS until `shutdown`. The
/// listener comes up inside the server task; `local_addr` waits for it.
pub async fn serve_tls(
    addr: SocketAddr,
    cert: &Path,
    key: &Path,
    router: Router,
) -> std::io::Result<TlsServer> {
    // rustls wants one process-wide provider; a second install (a test
    // running two daemons) answers `Err` and is harmless
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = RustlsConfig::from_pem_file(cert, key).await?;
    let handle: Handle<SocketAddr> = Handle::new();
    let h = handle.clone();
    let task = tokio::spawn(async move {
        axum_server::bind_rustls(addr, config)
            .handle(h)
            .serve(router.into_make_service())
            .await
    });
    Ok(TlsServer { handle, task })
}

impl TlsServer {
    /// The bound address, once listening; `None` if the server task ended
    /// first (a bind failure), which `shutdown` then reports.
    pub async fn local_addr(&self) -> Option<SocketAddr> {
        self.handle.listening().await
    }

    /// Stops accepting, gives in-flight requests five seconds, and
    /// returns the server task's result.
    pub async fn shutdown(self) -> std::io::Result<()> {
        self.handle
            .graceful_shutdown(Some(Duration::from_secs(5)));
        self.task
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?
    }
}
```

`kube/mod.rs`: `pub mod tls;` and `pub use tls::{TlsServer, serve_tls};`; `lib.rs`: add `TlsServer, serve_tls` to the `kube` re-export.

- [ ] **Step 5: The flags and the Kubernetes wiring**

`crates/balerix/src/cli.rs`, `ServeArgs`:

```rust
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
    /// `tmux` (the default): one machine. `kubernetes` (Spec O §7.3): TLS
    /// from --tls-cert and --tls-key, the admin token from
    /// --admin-token-file, any --bind address, no plugins.yaml, and the
    /// sidecar link in place of tmux.
    #[arg(long, value_enum, default_value_t = ServeMode::Tmux)]
    pub mode: ServeMode,
    /// The serving certificate chain, PEM (--mode kubernetes).
    #[arg(long, requires = "tls_key")]
    pub tls_cert: Option<PathBuf>,
    /// The serving key, PEM (--mode kubernetes).
    #[arg(long, requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,
    /// A file holding the admin token (--mode kubernetes).
    #[arg(long)]
    pub admin_token_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ServeMode {
    Tmux,
    Kubernetes,
}
```

`crates/balerix/src/commands/serve.rs`: `serve_command` becomes

```rust
pub fn serve_command(args: &ServeArgs) -> Result<String> {
    let layout = layout_from_env()?;
    let paths = server_paths(&layout);
    let config = ServerConfig::load(&layout.config_root.join("config.toml"))?;
    let bind = args.bind.clone().unwrap_or(config.bind);
    match args.mode {
        ServeMode::Tmux => {
            if args.tls_cert.is_some() || args.tls_key.is_some() || args.admin_token_file.is_some() {
                bail!("--tls-cert, --tls-key and --admin-token-file are for --mode kubernetes");
            }
            require_loopback(&bind)?;
            if args.detach {
                return detach(&paths, &bind, &args.tmux_socket);
            }
            run(&layout, &paths, &bind, &config.log, &args.tmux_socket, args.detached_child)
        }
        ServeMode::Kubernetes => {
            let (Some(cert), Some(key), Some(token_file)) =
                (&args.tls_cert, &args.tls_key, &args.admin_token_file)
            else {
                bail!("serve --mode kubernetes needs --tls-cert, --tls-key and --admin-token-file (Spec O §7.3)");
            };
            if args.detach {
                bail!("--detach is not available with --mode kubernetes; a pod runs the daemon in the foreground");
            }
            let addr: SocketAddr = bind
                .parse()
                .with_context(|| format!("bind address {bind:?} is not a valid host:port"))?;
            run_kubernetes(&layout, &paths, addr, &config.log, cert, key, token_file)
        }
    }
}
```

and a new function:

```rust
/// Spec O §7.3: TLS on the pod address, the operator's admin token, the
/// link hub as the runner and the workspace reader, no files to
/// materialise, a pool that is a Job's, and no `plugins.yaml` (§9 brings
/// `PUT /v1/plugins`). One process per pod, so there is no
/// `already_running` check and no detach.
#[allow(clippy::too_many_arguments)]
fn run_kubernetes(
    layout: &StateLayout,
    paths: &ServerPaths,
    addr: SocketAddr,
    log: &str,
    cert: &Path,
    key: &Path,
    token_file: &Path,
) -> Result<String> {
    init_tracing(paths, log, false)?;
    let token = std::fs::read_to_string(token_file)
        .with_context(|| format!("cannot read {}", token_file.display()))?
        .trim()
        .to_string();
    if token.len() < 32 {
        bail!("{}: the admin token is at least 32 characters", token_file.display());
    }
    let vault = Vault::load_or_create(&paths.vault_key())?;
    let store = FileFleetStore::new(layout.fleets_dir(), vault.clone());
    let existing = store.load_all()?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let hub = LinkHub::new();
        let ports = Ports {
            materializer: Arc::new(NoFiles),
            runner: hub.clone(),
            clock: Arc::new(SystemClock),
            store: Arc::new(store),
            workspace: hub.clone(),
            resolver: Arc::new(HostResolver),
            credentials: Arc::new(HostResolver),
            policy: ReconcilePolicy::default(),
            hook_url: format!("https://{addr}"),
            resync: RESYNC,
            kube: Some(hub),
        };
        let fleets = existing.len();
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
            PluginHostConfig {
                plugins_file: layout.config_root.join("plugins.yaml"),
                install_root: layout.plugins_data_dir(),
            },
            registry,
            client,
            kv,
            Arc::new(NoPool),
        );
        let server = serve_tls(addr, cert, key, router(daemon)).await?;
        let local = match server.local_addr().await {
            Some(a) => a,
            None => {
                server.shutdown().await?;
                bail!("cannot bind {addr}");
            }
        };
        let url = format!("https://{local}");
        write_pid(&paths.pid(), std::process::id())?;
        write_endpoint(&paths.endpoint(), &url)?;
        tracing::info!(%url, fleets, "balerix daemon listening (kubernetes mode)");
        eprintln!("listening on {url}");
        shutdown_signal().await;
        tracing::info!("shutting down");
        server.shutdown().await?;
        remove_if_exists(&paths.endpoint())?;
        remove_if_exists(&paths.pid())?;
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(String::new())
}
```

Imports: `use balerix_server::kube::{LinkHub, NoFiles, NoPool, serve_tls};`, `use crate::cli::{ServeArgs, ServeMode};`, `std::path::Path` is already imported. The existing `run` and `detach` are unchanged except `Ports { …, kube: None }` (Task 6 did that).

- [ ] **Step 6: Run the CLI tests, then everything**

Run: `mise x -- cargo test -p balerix --test cli_serve` then `mise run check`
Expected: 2 new tests pass; the existing `foreground_serve_writes_endpoint_and_answers_with_the_token` unchanged; `check` passes, `check-core-deps.sh` still reports `json`.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock crates/balerix-server crates/balerix
git commit -m "feat(cli): serve --mode kubernetes: TLS from a mounted certificate, the admin token from a file, no plugins.yaml (Spec O §7.3)"
```

---

### Task 10: The `agent/` project, `balerix-agent run`, the mise task, the CI job

**Files:**
- Create: `agent/Cargo.toml`, `agent/clippy.toml`, `agent/deny.toml`, `agent/src/lib.rs`, `agent/src/main.rs`, `agent/src/cli.rs`, `agent/src/run.rs`, `agent/tests/cli_it.rs`, `agent/tests/run_it.rs`, `agent/tests/support/mod.rs`
- Create: `scripts/agent.sh`
- Modify: `mise.toml` (`fmt`, `audit`, new `agent` task), `.github/workflows/ci.yml` (job `agent`), `.gitignore` if `agent/target` is not already covered by a `target/` pattern (check with `git check-ignore agent/target`)

**Interfaces:**
- Consumes: `balerix_runtime::ANCHOR_WINDOW` (`"balerix"`); `scripts/plugin.sh` as the model.
- Produces, for Tasks 11–13:
  - A standalone project `agent/` (package `balerix-agent`, version `0.2.0`, the core version), library plus binary: `balerix_agent::{cli, run}` now, `hooks`, `tls`, `link`, `attach`, `sidecar`, `bundle` later.
  - `balerix-agent run [--run-dir /balerix/run] [--start-timeout-secs 600]`: waits for `<run>/started` (content: the crew's session name), runs `tmux -S <run>/tmux.sock -u new-session -d -s <session> -n balerix -- /bin/sh -c 'while :; do sleep 3600; done'`, then polls `has-session` every second and exits 0 when the server is gone; on SIGTERM or SIGINT runs `kill-server` first. Exit 1 with `no start marker at <path> after <n> s` when the marker never comes.
  - `balerix-agent sidecar` with its default `--bundle` missing exits 1 with `cannot read the agent bundle at /balerix/secret/agent.json: …` on stderr and in `--termination-log` (§13's smoke test).
  - `mise run agent` = `scripts/agent.sh check`: builds `balerix` from the core workspace, exports `BALERIX_BIN`, then fmt-check, clippy and nextest in `agent/`.
  - CI job `agent`, with `BALERIX_REQUIRE_TOOLS=1` and the tools installed, concurrent with `check` and `plugins`.
  - `agent/tests/support/mod.rs`: `tools() -> Option<Tools>` (`Tools { tmux, git, mise, nono, balerix: PathBuf }`; `balerix` from `BALERIX_BIN`), `require_or_skip(name, present) -> bool`, `temp_root(label) -> PathBuf` under `CARGO_TARGET_TMPDIR`, `landlock_works(tools, root) -> bool`, `wait_for(f)`.

- [ ] **Step 1: The project files**

`agent/Cargo.toml`:

```toml
# A standalone project, not a workspace member (Spec H, Spec O-12): its
# TLS stack, WebSocket client and their trees stay out of the core
# workspace's feature resolution. Reaches the core crates by path.
[workspace]
resolver = "3"

[package]
name = "balerix-agent"
description = "The balerix agent pod: the sidecar that materialises and drives one agent, and the agent container's entrypoint (Spec O §6, §7)"
# The core version (O-13): scripts/release/prepare.sh writes it with the
# other core manifests once sub-project 5 lands.
version = "0.2.0"
edition = "2024"
rust-version = "1.98"
license = "Apache-2.0"
repository = "https://github.com/balerix-ai/balerix"
publish = false

[lib]
name = "balerix_agent"
path = "src/lib.rs"

[[bin]]
name = "balerix-agent"
path = "src/main.rs"

[dependencies]
balerix-api = { path = "../crates/balerix-api" }
balerix-core = { path = "../crates/balerix-core" }
balerix-runtime = { path = "../crates/balerix-runtime" }
anyhow = "1.0.104"
clap = { version = "4.6.6", features = ["derive"] }
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
thiserror = "2.0.20"
tokio = { version = "1.53.1", features = ["rt-multi-thread", "macros", "sync", "time", "signal", "net", "fs"] }
# The hook listener on 127.0.0.1 (§7.1): plain HTTP, as the daemon's own
# ingress is on one machine. `ws` for the tests' fake Daemon.
axum = { version = "0.8.9", features = ["ws"] }
# The hook forwarder to the Daemon (§7.1) over TLS from the mounted
# authority (§10.3): rustls with no provider of its own, ring below.
reqwest = { version = "0.13.4", default-features = false, features = ["json", "rustls-no-provider"] }
rustls = { version = "0.23.45", default-features = false, features = ["ring", "std", "tls12", "logging"] }
rustls-pki-types = { version = "1.15.1", features = ["std"] }
# The link (§7.2). `rustls-tls-webpki-roots` is the feature that enables
# `Connector::Rustls`; the bundled roots go unused, since the Daemon's
# authority is the only one trusted.
tokio-tungstenite = { version = "0.30.0", features = ["rustls-tls-webpki-roots"] }
futures-util = { version = "0.3.34", default-features = false, features = ["sink", "std"] }
tracing = "0.1.44"
tracing-subscriber = { version = "0.3.23", features = ["env-filter"] }

[dev-dependencies]
# a throwaway authority for the two-process TLS test
rcgen = { version = "0.14.10", default-features = false, features = ["pem", "ring"] }
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

`agent/clippy.toml`: the two lines of `plugins/flow/clippy.toml` (`allow-unwrap-in-tests = true`, `allow-expect-in-tests = true`). `agent/deny.toml`: `plugins/github/deny.toml` verbatim, with the header comment changed to `# The agent project's own policy (Spec H, Spec O-12). CDLA-Permissive-2.0 is the Mozilla root bundle webpki-roots, pulled by tokio-tungstenite's rustls feature and never consulted.`

`agent/src/lib.rs`:

```rust
//! The balerix agent pod (Spec O §6, §7). `sidecar` materialises one agent
//! on the claim with `balerix-runtime`, drives it over a tmux socket the
//! `agent` container's server listens on, forwards Claude's hooks to the
//! Daemon and holds one outbound link to it. `run` is the agent
//! container's entrypoint: it starts that tmux server once the sidecar
//! says the agent is ready to launch.

pub mod cli;
pub mod run;
```

`agent/src/main.rs`:

```rust
use std::process::ExitCode;

use balerix_agent::cli::{Cli, Command};
use clap::Parser;

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
    match cli.command {
        Command::Run(args) => match balerix_agent::run::run(&args) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("balerix-agent run: {e:#}");
                ExitCode::FAILURE
            }
        },
        Command::Sidecar(args) => {
            // Task 13 replaces this arm with `sidecar::main(args)`; until
            // then the one thing a sidecar does without a bundle is refuse.
            let termination_log = args.termination_log.clone();
            let e = anyhow::anyhow!(
                "cannot read the agent bundle at {}: {}",
                args.bundle.display(),
                std::fs::read(&args.bundle)
                    .err()
                    .map_or("the sidecar is not implemented yet".to_string(), |e| e.to_string())
            );
            balerix_agent::cli::terminate(&termination_log, &e)
        }
    }
}
```

`agent/src/cli.rs`:

```rust
//! The command line. Mount paths carry Spec O §6.1's defaults; tests point
//! them at temp roots.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "balerix-agent",
    version,
    about = "The balerix agent pod: the sidecar and the agent container's entrypoint (Spec O §6)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// The sidecar: materialise the agent, link to the Daemon, forward hooks.
    Sidecar(SidecarArgs),
    /// The agent container: start the tmux server once the sidecar is ready.
    Run(RunArgs),
}

#[derive(Debug, Args)]
pub struct SidecarArgs {
    /// The agent bundle the operator mounted: the resolved agent, the
    /// credentials and the Daemon token (Spec O §5.4).
    #[arg(long, default_value = "/balerix/secret/agent.json")]
    pub bundle: PathBuf,
    /// The authority that signed the Daemon's certificate (§10.3).
    #[arg(long, default_value = "/balerix/tls/ca.crt")]
    pub ca: PathBuf,
    /// The agent claim (§6.1).
    #[arg(long, default_value = "/balerix/agent")]
    pub agent_dir: PathBuf,
    /// The read-only crew slice: repo/.git/objects and the three pools.
    #[arg(long, default_value = "/balerix/shared")]
    pub shared_dir: PathBuf,
    /// The run directory both containers mount: the tmux socket, the markers.
    #[arg(long, default_value = "/balerix/run")]
    pub run_dir: PathBuf,
    /// Claude posts hooks to 127.0.0.1 on this port (§7.1); 0 picks a free one.
    #[arg(long, default_value_t = 7643)]
    pub hook_port: u16,
    /// The `balerix` binary launch.sh runs (agent-supervise, hook-relay);
    /// found on PATH when absent.
    #[arg(long)]
    pub balerix: Option<PathBuf>,
    /// Where a start-up failure's one-line reason goes (the pod reads it
    /// as the container's termination message, §6.2 step 2).
    #[arg(long, default_value = "/dev/termination-log")]
    pub termination_log: PathBuf,
    /// Accept an http:// daemon_url. Tests only; a pod's Daemon is https.
    #[arg(long, hide = true)]
    pub allow_plain_http: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[arg(long, default_value = "/balerix/run")]
    pub run_dir: PathBuf,
    /// How long to wait for the sidecar's start marker.
    #[arg(long, default_value_t = 600)]
    pub start_timeout_secs: u64,
}

/// Ends the sidecar with its reason as the termination message: the first
/// line of `e`, written to `log` (best effort: the path may not exist off
/// a pod) and to stderr. Always exit 1.
pub fn terminate(log: &Path, e: &anyhow::Error) -> ExitCode {
    let reason = format!("{e:#}");
    let first = reason.lines().next().unwrap_or("").to_string();
    if let Err(write) = std::fs::write(log, format!("{first}\n")) {
        tracing::debug!(path = %log.display(), "no termination log: {write}");
    }
    eprintln!("balerix-agent sidecar: {reason}");
    ExitCode::FAILURE
}
```

`agent/src/run.rs`:

```rust
//! `balerix-agent run`: the agent container's entrypoint (Spec O §6.2
//! step 3). Waits for the sidecar's start marker, starts the tmux server
//! on the shared socket with the crew's anchor window (what the runner's
//! `ensure_crew` would create), and lives as long as the server does. The
//! sidecar then creates the agent's window through the socket exactly as
//! the daemon does on one machine.
//!
//! It is the container's pid 1. The server daemonises away from it, so it
//! polls `has-session`; SIGTERM (the pod ending) becomes `kill-server`,
//! and the supervisor in the pane ends the agent's tree on the hangup
//! that follows (Spec N amendment §13).

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use balerix_runtime::ANCHOR_WINDOW;

use crate::cli::RunArgs;

const POLL: Duration = Duration::from_millis(200);
const IDLE: &str = "while :; do sleep 3600; done";

pub fn run(args: &RunArgs) -> Result<ExitCode> {
    let marker = args.run_dir.join("started");
    let socket = args.run_dir.join("tmux.sock");
    let session = wait_marker(&marker, Duration::from_secs(args.start_timeout_secs))?;
    let tmux = on_path("tmux").context("tmux is not on PATH")?;
    let status = Command::new(&tmux)
        .arg("-S")
        .arg(&socket)
        .args(["-u", "new-session", "-d", "-s", &session, "-n", ANCHOR_WINDOW, "--", "/bin/sh", "-c", IDLE])
        .status()
        .context("cannot start tmux")?;
    ensure!(status.success(), "tmux new-session exited {status}");
    tracing::info!(%session, socket = %socket.display(), "tmux server up");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    if !has_session(&tmux, &socket, &session) {
                        tracing::info!("tmux server gone; exiting");
                        return Ok::<(), anyhow::Error>(());
                    }
                }
                _ = term.recv() => break,
                _ = int.recv() => break,
            }
        }
        tracing::info!("signal received; killing the tmux server");
        let _ = Command::new(&tmux).arg("-S").arg(&socket).args(["-u", "kill-server"]).status();
        Ok(())
    })?;
    Ok(ExitCode::SUCCESS)
}

/// The marker's trimmed content: the crew's session name.
fn wait_marker(marker: &Path, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(s) = std::fs::read_to_string(marker)
            && !s.trim().is_empty()
        {
            return Ok(s.trim().to_string());
        }
        if Instant::now() >= deadline {
            bail!(
                "no start marker at {} after {} s",
                marker.display(),
                timeout.as_secs()
            );
        }
        std::thread::sleep(POLL);
    }
}

fn has_session(tmux: &Path, socket: &Path, session: &str) -> bool {
    Command::new(tmux)
        .arg("-S")
        .arg(socket)
        .args(["-u", "has-session", "-t", &format!("={session}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The first executable `name` on PATH.
pub fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}
```

- [ ] **Step 2: Write the failing tests**

`agent/tests/support/mod.rs`:

```rust
//! Shared scaffolding for the agent project's integration tests: the real
//! tools (skipped, or failed under `BALERIX_REQUIRE_TOOLS=1`), temp roots
//! under `target/tmp`, and the `balerix` binary `scripts/agent.sh` built.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct Tools {
    pub tmux: PathBuf,
    pub git: PathBuf,
    pub mise: PathBuf,
    pub nono: PathBuf,
    /// From `BALERIX_BIN` (`scripts/agent.sh check` sets it).
    pub balerix: PathBuf,
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Every tool, or `None` after printing a skip (a failure under
/// `BALERIX_REQUIRE_TOOLS=1`).
pub fn tools() -> Option<Tools> {
    let balerix = std::env::var_os("BALERIX_BIN").map(PathBuf::from);
    let found = (|| {
        Some(Tools {
            tmux: on_path("tmux")?,
            git: on_path("git")?,
            mise: on_path("mise")?,
            nono: on_path("nono")?,
            balerix: balerix.filter(|p| p.is_file())?,
        })
    })();
    if found.is_none() {
        assert!(!require_or_skip(
            "tmux+git+mise+nono on PATH and BALERIX_BIN (run through scripts/agent.sh)",
            false
        ));
    }
    found
}

/// Prints `skip: <name> missing` and returns false, or panics when
/// `BALERIX_REQUIRE_TOOLS=1`.
pub fn require_or_skip(name: &str, present: bool) -> bool {
    if present {
        return true;
    }
    if std::env::var("BALERIX_REQUIRE_TOOLS").as_deref() == Ok("1") {
        panic!("{name} missing and BALERIX_REQUIRE_TOOLS=1");
    }
    eprintln!("skip: {name} missing");
    false
}

/// `<target/tmp>/<label>-<pid>`, created fresh. Never `/tmp`: nono grants
/// it by default.
pub fn temp_root(label: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// `nono -s run` with a one-directory profile can write there: Landlock
/// is on. The same probe `balerix-runtime`'s tests use.
pub fn landlock_works(tools: &Tools, root: &Path) -> bool {
    let dir = root.join("landlock-probe");
    std::fs::create_dir_all(&dir).unwrap();
    let profile = dir.join("profile.json");
    std::fs::write(
        &profile,
        serde_json::json!({
            "filesystem": { "read": ["/usr", "/lib", "/lib64", "/bin", "/etc"], "allow": [dir.display().to_string()] },
            "workdir": { "access": "none" }
        })
        .to_string(),
    )
    .unwrap();
    std::process::Command::new(&tools.nono)
        .args(["-s", "run", "--profile"])
        .arg(&profile)
        .args(["--", "/bin/sh", "-c", &format!("echo ok > {}/ok", dir.display())])
        .env("HOME", &dir)
        .status()
        .is_ok_and(|s| s.success())
        && dir.join("ok").is_file()
}

pub fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}
```

Compare `landlock_works` with `crates/balerix-runtime/tests/support/mod.rs:54` and copy its profile shape exactly if it differs (nono 0.79.0 rejects unknown keys).

`agent/tests/cli_it.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_balerix-agent");

#[test]
fn version_prints_the_core_version() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("balerix-agent {}", env!("CARGO_PKG_VERSION"))
    );
}

/// Spec O §13's smoke test: a sidecar with no configuration exits 1 with
/// a message, and the message is the termination log's one line.
#[test]
fn a_sidecar_without_a_bundle_exits_one_with_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("termination-log");
    let out = Command::new(BIN)
        .args(["sidecar", "--bundle", "/nonexistent/agent.json", "--termination-log"])
        .arg(&log)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot read the agent bundle at /nonexistent/agent.json"),
        "{stderr}"
    );
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(logged.starts_with("cannot read the agent bundle at /nonexistent/agent.json"));
    assert_eq!(logged.lines().count(), 1);
}
```

`agent/tests/run_it.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §6.2 step 3: the agent container waits for the sidecar's
//! marker, starts the tmux server on the shared socket, and lives as long
//! as the server does.
mod support;

use std::process::{Child, Command, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_balerix-agent");

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn tmux(tools: &support::Tools, socket: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = Command::new(&tools.tmux)
        .arg("-S")
        .arg(socket)
        .arg("-u")
        .args(args)
        .output()
        .unwrap();
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn run_waits_for_the_marker_starts_the_server_and_ends_with_it() {
    let Some(tools) = support::tools() else { return };
    let root = support::temp_root("run");
    let run_dir = root.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let socket = run_dir.join("tmux.sock");
    let child = Command::new(BIN)
        .args(["run", "--start-timeout-secs", "20", "--run-dir"])
        .arg(&run_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut child = Kill(child);
    std::thread::sleep(Duration::from_millis(500));
    assert!(child.0.try_wait().unwrap().is_none(), "exited before the marker");
    assert!(!socket.exists(), "no server before the marker");

    std::fs::write(run_dir.join("started"), "f/c\n").unwrap();
    support::wait_for("the socket", Duration::from_secs(10), || socket.exists());
    support::wait_for("the anchor window", Duration::from_secs(10), || {
        tmux(&tools, &socket, &["list-windows", "-t", "=f/c", "-F", "#{window_name}"])
            .is_some_and(|w| w.trim() == "balerix")
    });
    assert!(child.0.try_wait().unwrap().is_none(), "stays while the server lives");

    tmux(&tools, &socket, &["kill-server"]);
    support::wait_for("run to exit", Duration::from_secs(5), || {
        child.0.try_wait().unwrap().is_some()
    });
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn run_gives_up_without_a_marker() {
    let Some(_tools) = support::tools() else { return };
    let root = support::temp_root("run-nomarker");
    let run_dir = root.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let out = Command::new(BIN)
        .args(["run", "--start-timeout-secs", "1", "--run-dir"])
        .arg(&run_dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains(&format!(
        "no start marker at {} after 1 s",
        run_dir.join("started").display()
    )));
}

#[test]
fn a_sigterm_ends_the_server() {
    let Some(tools) = support::tools() else { return };
    let root = support::temp_root("run-term");
    let run_dir = root.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("started"), "f/c\n").unwrap();
    let socket = run_dir.join("tmux.sock");
    let child = Command::new(BIN)
        .args(["run", "--run-dir"])
        .arg(&run_dir)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = Kill(child);
    support::wait_for("the server", Duration::from_secs(10), || {
        tmux(&tools, &socket, &["has-session", "-t", "=f/c"]).is_some()
    });
    assert!(
        Command::new("kill")
            .args(["-TERM", &child.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    support::wait_for("run to exit", Duration::from_secs(5), || {
        child.0.try_wait().unwrap().is_some()
    });
    assert!(tmux(&tools, &socket, &["has-session", "-t", "=f/c"]).is_none(), "the server is gone");
}
```

- [ ] **Step 3: The script, the task, the job**

`scripts/agent.sh`:

```bash
#!/usr/bin/env bash
# Build, format, lint and test the standalone agent project (Spec O §12),
# the way scripts/plugin.sh does a plugin: its own cargo workspace, its own
# lockfile, its own target directory. The sidecar's two-process tests need
# the `balerix` binary (launch.sh runs `balerix agent-supervise` and
# `balerix hook-relay`; `balerix serve --mode kubernetes` is the Daemon
# under test), so `check` builds it from the core workspace first and
# hands its path over as BALERIX_BIN; without it those tests skip (fail
# under BALERIX_REQUIRE_TOOLS=1).
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"

usage() { echo "usage: $0 {build|fmt|check}" >&2; exit 2; }
[[ $# -eq 1 ]] || usage
dir="$repo/agent"
target="$dir/target"

case "$1" in
  build)
    CARGO_TARGET_DIR="$target" cargo build -q --manifest-path "$dir/Cargo.toml"
    ;;
  fmt)
    CARGO_TARGET_DIR="$target" cargo fmt --manifest-path "$dir/Cargo.toml" --all
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

`chmod +x scripts/agent.sh`. `mise.toml`:

- `[tasks.fmt]` `run` list: append `"scripts/agent.sh fmt",`.
- `[tasks.audit]` `run` list: append `"cargo audit --file agent/Cargo.lock",` and `"cargo deny --manifest-path agent/Cargo.toml check advisories bans sources licenses",`.
- After `[tasks.plugins]`:
  ```toml
  [tasks.agent]
  description = "Lint and test the standalone agent project (balerix-agent, Spec O §12): builds balerix first for its two-process tests. Its own tier; not part of `check`"
  run = "scripts/agent.sh check"
  ```

`.github/workflows/ci.yml`, after the `plugins` job:

```yaml
  # The agent project (Spec O §12) is standalone like a plugin, with one
  # difference: its two-process tests run the core `balerix` binary, so
  # the job carries the core target cache too and the real tools.
  agent:
    runs-on: ubuntu-latest
    env:
      BALERIX_REQUIRE_TOOLS: "1"
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with: { version: 2026.9.2, install: false, cache: true }
      - run: mise install rust cargo:cargo-nextest tmux nono gh
      - uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
        with:
          workspaces: |
            . -> target
            agent -> target
          key: agent
      - run: mise run agent
```

Run `mise x -- actionlint` and `mise x -- zizmor --offline --min-severity medium .github` (both are in `lint`).

- [ ] **Step 4: Run the project's tier**

Run: `mise run agent`
Expected: `balerix` builds; `cli_it` passes (2); `run_it` passes (3) on this machine (tmux present); `cargo deny` is not part of it, so run `mise x -- cargo deny --manifest-path agent/Cargo.toml check licenses` once: passes with `CDLA-Permissive-2.0` allowed.

- [ ] **Step 5: Gate and commit**

Run: `mise run check` (unchanged core) and `mise run agent`.

```bash
git add agent scripts/agent.sh mise.toml .github/workflows/ci.yml
git commit -m "feat(agent): the balerix-agent project and its run entrypoint, with mise run agent and a CI job (Spec O §6.2, §12)"
```

---

### Task 11: The sidecar's hook ingress and its trust

**Files:**
- Create: `agent/src/tls.rs`, `agent/src/hooks.rs`, `agent/tests/hooks_it.rs`
- Modify: `agent/src/lib.rs`

**Interfaces:**
- Consumes: `SidecarArgs.ca`, `hook_port` (Task 10); the Daemon's hook route shape (`POST /v1/agents/{f}/{c}/{a}/events`, `Authorization: Bearer`, `{"error":…}` bodies).
- Produces, for Tasks 12–13:
  - `balerix_agent::tls::client_config(ca: &Path) -> anyhow::Result<Arc<rustls::ClientConfig>>` (the one authority, ring provider); `balerix_agent::tls::http_client(tls: &Arc<rustls::ClientConfig>, timeout: Duration) -> anyhow::Result<reqwest::Client>`.
  - `balerix_agent::hooks::{Hooks, router, FORWARD_BUDGET, BODY_LIMIT, constant_time_eq}`: `Hooks { id: AgentId, token: String, daemon_url: String, http: reqwest::Client, failures: Arc<AtomicU64>, events: mpsc::UnboundedSender<String> }`; `router(Arc<Hooks>) -> axum::Router` serving `POST /v1/agents/{fleet}/{crew}/{agent}/events`.
  - Behaviour: 401 `unknown agent or bad secret` for another agent's path or a bad token; 400 for a non-object body or a missing `hook_event_name` (the Daemon's texts); every accepted event's name goes to `events`; the body is forwarded verbatim with the token to `<daemon_url>/v1/agents/<id>/events`; the Daemon's status and body come back (`200 {}` for an empty 2xx body); no answer within `FORWARD_BUDGET` (3 s), or a connection error, answers `200 {}` and increments `failures`.

- [ ] **Step 1: Write the failing test**

Create `agent/tests/hooks_it.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §7.1: the sidecar forwards hooks and fails open.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use std::future::IntoFuture;

use balerix_agent::hooks::{FORWARD_BUDGET, Hooks, router};
use balerix_agent::tls::{client_config, http_client};
use balerix_core::AgentId;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// A fake Daemon: records what arrives, answers a verdict, optionally
/// after a sleep longer than the budget.
#[derive(Default)]
struct Seen(Mutex<Vec<(Option<String>, Value)>>);

async fn fake_events(
    State((seen, slow)): State<(Arc<Seen>, bool)>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    if slow {
        tokio::time::sleep(FORWARD_BUDGET + Duration::from_secs(2)).await;
    }
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    seen.0
        .lock()
        .await
        .push((auth, serde_json::from_slice(&body).unwrap()));
    axum::Json(json!({ "decision": "block", "reason": "no" })).into_response()
}
use axum::response::IntoResponse;

async fn fake_daemon(slow: bool) -> (Arc<Seen>, String) {
    let seen = Arc::new(Seen::default());
    let app = Router::new()
        .route("/v1/agents/{f}/{c}/{a}/events", post(fake_events))
        .with_state((seen.clone(), slow));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app).into_future());
    (seen, url)
}

fn ca_file(dir: &std::path::Path) -> std::path::PathBuf {
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = ca.self_signed(&key).unwrap();
    let path = dir.join("ca.crt");
    std::fs::write(&path, cert.pem()).unwrap();
    path
}

async fn sidecar(daemon_url: &str) -> (String, Arc<AtomicU64>, mpsc::UnboundedReceiver<String>) {
    let dir = tempfile::tempdir().unwrap();
    let tls = client_config(&ca_file(dir.path())).unwrap();
    let (events, rx) = mpsc::unbounded_channel();
    let failures = Arc::new(AtomicU64::new(0));
    let hooks = Arc::new(Hooks {
        id: "f/c/a".parse::<AgentId>().unwrap(),
        token: TOKEN.into(),
        daemon_url: daemon_url.into(),
        http: http_client(&tls, Duration::from_secs(10)).unwrap(),
        failures: failures.clone(),
        events,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, router(hooks)).into_future());
    (url, failures, rx)
}

async fn post(base: &str, path: &str, token: Option<&str>, body: &str) -> (u16, String) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut req = client
        .post(format!("{base}{path}"))
        .header("content-type", "application/json")
        .body(body.to_string());
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.unwrap();
    (resp.status().as_u16(), resp.text().await.unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_with_the_token_and_returns_the_daemons_answer() {
    let (seen, daemon) = fake_daemon(false).await;
    let (base, failures, mut names) = sidecar(&daemon).await;
    let body = r#"{"hook_event_name":"PreToolUse","session_id":"s1","tool_input":{"command":"ls"}}"#;
    let (status, text) = post(&base, "/v1/agents/f/c/a/events", Some(TOKEN), body).await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap(),
        json!({ "decision": "block", "reason": "no" })
    );
    let got = seen.0.lock().await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0.as_deref(), Some(&*format!("Bearer {TOKEN}")));
    assert_eq!(got[0].1, serde_json::from_str::<Value>(body).unwrap());
    assert_eq!(names.recv().await.unwrap(), "PreToolUse");
    assert_eq!(failures.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refuses_another_agent_a_bad_token_and_a_bad_body() {
    let (_seen, daemon) = fake_daemon(false).await;
    let (base, _, _) = sidecar(&daemon).await;
    let ok = r#"{"hook_event_name":"Stop"}"#;
    assert_eq!(
        post(&base, "/v1/agents/f/c/b/events", Some(TOKEN), ok).await,
        (401, r#"{"error":"unknown agent or bad secret"}"#.into())
    );
    assert_eq!(post(&base, "/v1/agents/f/c/a/events", Some("nope"), ok).await.0, 401);
    assert_eq!(post(&base, "/v1/agents/f/c/a/events", None, ok).await.0, 401);
    assert_eq!(
        post(&base, "/v1/agents/f/c/a/events", Some(TOKEN), "[1]").await,
        (400, r#"{"error":"body must be a JSON object"}"#.into())
    );
    assert_eq!(
        post(&base, "/v1/agents/f/c/a/events", Some(TOKEN), r#"{"x":1}"#).await,
        (400, r#"{"error":"hook_event_name must be a string"}"#.into())
    );
}

/// Review Focus 4: a Daemon that is down, and one that is too slow, both
/// get the empty chain's answer inside the budget, and are counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_that_is_down_or_slow_fails_open_inside_the_budget() {
    // down: a port nobody listens on
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let (base, failures, _) = sidecar(&closed).await;
    let start = Instant::now();
    let (status, text) = post(&base, "/v1/agents/f/c/a/events", Some(TOKEN), r#"{"hook_event_name":"Stop"}"#).await;
    assert_eq!((status, text.as_str()), (200, "{}"));
    assert!(start.elapsed() < FORWARD_BUDGET, "{:?}", start.elapsed());
    assert_eq!(failures.load(Ordering::Relaxed), 1);

    // slow: answers after the budget
    let (_seen, slow) = fake_daemon(true).await;
    let (base, failures, _) = sidecar(&slow).await;
    let start = Instant::now();
    let (status, text) = post(&base, "/v1/agents/f/c/a/events", Some(TOKEN), r#"{"hook_event_name":"Stop"}"#).await;
    assert_eq!((status, text.as_str()), (200, "{}"));
    assert!(
        start.elapsed() < FORWARD_BUDGET + Duration::from_millis(500),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(failures.load(Ordering::Relaxed), 1);
}
```

- [ ] **Step 2: Run to see it fail**

Run: `CARGO_TARGET_DIR=agent/target mise x -- cargo test --manifest-path agent/Cargo.toml --test hooks_it`
Expected: compile errors (`balerix_agent::hooks`, `balerix_agent::tls` missing).

- [ ] **Step 3: `tls.rs`**

```rust
//! Trust for the Daemon (Spec O §10.3): one authority, the mounted file,
//! and nothing else; rustls on the ring provider, shared by the hook
//! forwarder (reqwest) and the link (tokio-tungstenite).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use rustls_pki_types::pem::PemObject;

pub fn client_config(ca: &Path) -> Result<Arc<rustls::ClientConfig>> {
    // one process-wide provider; a second install is a harmless `Err`
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    let mut count = 0;
    let certs = rustls_pki_types::CertificateDer::pem_file_iter(ca)
        .with_context(|| format!("cannot read the authority at {}", ca.display()))?;
    for cert in certs {
        let cert = cert.with_context(|| format!("{}: not a PEM certificate", ca.display()))?;
        roots
            .add(cert)
            .with_context(|| format!("{}: not a usable certificate", ca.display()))?;
        count += 1;
    }
    ensure!(count > 0, "{}: no certificate in the authority file", ca.display());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

/// An HTTP client trusting `tls` alone, no proxy, one overall timeout.
/// `http://` URLs (tests, `--allow-plain-http`) never touch it.
pub fn http_client(tls: &Arc<rustls::ClientConfig>, timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .use_preconfigured_tls((**tls).clone())
        .no_proxy()
        .timeout(timeout)
        .build()?)
}
```

- [ ] **Step 4: `hooks.rs`**

```rust
//! The sidecar's hook ingress (Spec O §7.1). Claude posts to
//! `127.0.0.1:<port>` with the agent's token, exactly as it posts to the
//! daemon on one machine: the same `settings.json` shape, the same
//! `open_port` grant, the same `hook-relay` for `SessionStart`. Each event
//! is forwarded to the Daemon's events route with the token, and the
//! Daemon's answer returned. A Daemon that cannot be reached inside the
//! budget gets the empty chain's answer, `200 {}`, and the failure is
//! counted: a Daemon outage degrades the fleet, it never breaks the agent.
//! Every accepted event's name also goes to the sidecar's loop, which
//! turns `SessionStart` into `Ready`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use balerix_core::AgentId;
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// Under `hook-relay`'s 5 s and Claude's 10 s command timeout; above the
/// Daemon's own 2 s handler timeout, so a slow chain is the Daemon's
/// answer and not ours.
pub const FORWARD_BUDGET: Duration = Duration::from_millis(3000);
/// The Daemon's own cap on an event body.
pub const BODY_LIMIT: usize = 1 << 20;

pub struct Hooks {
    pub id: AgentId,
    pub token: String,
    /// `https://host:port`, no path.
    pub daemon_url: String,
    pub http: reqwest::Client,
    /// Events answered for a Daemon that could not be reached.
    pub failures: Arc<AtomicU64>,
    /// Every accepted event's `hook_event_name`.
    pub events: mpsc::UnboundedSender<String>,
}

pub fn router(hooks: Arc<Hooks>) -> Router {
    Router::new()
        .route("/v1/agents/{fleet}/{crew}/{agent}/events", post(events))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(hooks)
}

/// Length-then-bytes comparison with no early exit (as the daemon's
/// `auth::constant_time_eq`).
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

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get("authorization")?.to_str().ok()?;
    let (scheme, rest) = v.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let tok = rest.trim();
    (!tok.is_empty()).then_some(tok)
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

async fn events(
    State(h): State<Arc<Hooks>>,
    Path((fleet, crew, agent)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let unauthorized = || error(StatusCode::UNAUTHORIZED, "unknown agent or bad secret");
    if format!("{fleet}/{crew}/{agent}") != h.id.to_string() {
        return unauthorized();
    }
    let Some(token) = bearer(&headers) else {
        return unauthorized();
    };
    if !constant_time_eq(token.as_bytes(), h.token.as_bytes()) {
        return unauthorized();
    }
    let name = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Object(map)) => match map.get("hook_event_name") {
            Some(Value::String(s)) => s.clone(),
            _ => return error(StatusCode::BAD_REQUEST, "hook_event_name must be a string"),
        },
        Ok(_) => return error(StatusCode::BAD_REQUEST, "body must be a JSON object"),
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("body is not JSON: {e}")),
    };
    let _ = h.events.send(name.clone());
    let url = format!(
        "{}/v1/agents/{}/events",
        h.daemon_url.trim_end_matches('/'),
        h.id
    );
    let sent = tokio::time::timeout(
        FORWARD_BUDGET,
        h.http
            .post(&url)
            .bearer_auth(&h.token)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send(),
    )
    .await;
    match sent {
        Ok(Ok(resp)) => {
            let status = resp.status().as_u16();
            let bytes = match tokio::time::timeout(FORWARD_BUDGET, resp.bytes()).await {
                Ok(Ok(b)) => b,
                _ => return fail_open(&h, &name, "the Daemon's reply did not arrive"),
            };
            if (200..300).contains(&status) && bytes.is_empty() {
                return Json(json!({})).into_response();
            }
            (
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                [(CONTENT_TYPE, "application/json")],
                bytes,
            )
                .into_response()
        }
        Ok(Err(e)) => fail_open(&h, &name, &e.to_string()),
        Err(_) => fail_open(&h, &name, "no answer within the budget"),
    }
}

fn fail_open(h: &Hooks, name: &str, why: &str) -> Response {
    h.failures.fetch_add(1, Ordering::Relaxed);
    tracing::warn!(event = name, "hook not forwarded, answered as an empty chain: {why}");
    Json(json!({})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_compares_whole_slices() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
```

Add `pub mod hooks;` and `pub mod tls;` to `agent/src/lib.rs`.

- [ ] **Step 5: Run the tests**

Run: `mise run agent`
Expected: `hooks_it` 3 passed, everything else green.

- [ ] **Step 6: Commit**

```bash
git add agent
git commit -m "feat(agent): the sidecar's hook ingress forwards to the Daemon with the agent's token and fails open (Spec O §7.1)"
```

---

### Task 12: The sidecar's link and its attach bridge

**Files:**
- Create: `agent/src/link.rs`, `agent/src/attach.rs`, `agent/tests/link_it.rs`
- Modify: `agent/src/lib.rs`

**Interfaces:**
- Consumes: `tls::client_config` (Task 11); `AgentRunner`, `WorkspaceReader`, `PtyStream`, `balerix_core::fakes::FakeRunner` (core, always compiled); the frames (Task 2); the Daemon's two routes and their headers (Tasks 6, 8).
- Produces, for Task 13:
  - `balerix_agent::link::{LinkDeps, Control, run, connect, ws_url, dispatch, workspace_failure, RECONNECT_MIN, RECONNECT_MAX, Ws}`:
    ```rust
    pub struct LinkDeps {
        pub id: AgentId,
        pub token: String,
        pub daemon_url: String,
        pub tls: Arc<rustls::ClientConfig>,
        pub runner: Arc<dyn AgentRunner>,
        pub workspace: Arc<dyn WorkspaceReader>,
        pub control: mpsc::UnboundedSender<Control>,
        pub status: watch::Receiver<LinkStatus>,
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Control { Stop, Restart }
    pub async fn run(deps: Arc<LinkDeps>)   // never returns
    ```
    `run` connects to `ws(s)://<daemon>/v1/agents/<id>/link` with `Authorization: Bearer <token>` and `balerix-link-protocol: 1`, sends the current status first, then a status frame on every change of `status`, answers each request by id, and reconnects with back-off 1 s → 30 s.
  - `balerix_agent::attach::{open, bridge}`: on `Attach { session }` the sidecar attaches through the runner, opens `…/link/attach/<session>`, answers `Ok`, and bridges PTY bytes to binary frames both ways, the Daemon's resize text frame to `resize`, and the Daemon's close to the stream's drop.

- [ ] **Step 1: Write the failing test**

Create `agent/tests/link_it.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §7.2 from the sidecar's side, against a fake Daemon on plain
//! `ws://`: connect, status, requests and replies, attach, reconnect.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::ws::{Message as AxMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use std::future::IntoFuture;

use balerix_agent::link::{Control, LinkDeps, run};
use balerix_agent::tls::client_config;
use balerix_api::{
    AgentPhase, AgentStatus, FailureKind, LinkOp, LinkRequest, LinkResult, LinkStatus,
    SidecarFrame, WorkspaceDiff, WorkspaceTree, WorkspaceVersion,
};
use balerix_core::fakes::FakeRunner;
use balerix_core::{AgentId, WorkspaceError, WorkspaceReader};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// One link connection as the fake Daemon sees it.
struct Conn {
    headers: HeaderMap,
    to_sidecar: mpsc::UnboundedSender<LinkRequest>,
    from_sidecar: mpsc::UnboundedReceiver<SidecarFrame>,
}

/// One attach socket as the fake Daemon sees it.
struct AttachConn {
    headers: HeaderMap,
    session: String,
    to_sidecar: mpsc::UnboundedSender<AxMsg>,
    from_sidecar: mpsc::UnboundedReceiver<AxMsg>,
}

#[derive(Clone)]
struct Fake {
    links: mpsc::UnboundedSender<Conn>,
    attaches: mpsc::UnboundedSender<AttachConn>,
}

async fn link_route(State(f): State<Fake>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |mut socket: WebSocket| async move {
        let (to_tx, mut to_rx) = mpsc::unbounded_channel::<LinkRequest>();
        let (from_tx, from_rx) = mpsc::unbounded_channel::<SidecarFrame>();
        let _ = f.links.send(Conn {
            headers,
            to_sidecar: to_tx,
            from_sidecar: from_rx,
        });
        loop {
            tokio::select! {
                req = to_rx.recv() => match req {
                    Some(r) => { let _ = socket.send(AxMsg::Text(serde_json::to_string(&r).unwrap().into())).await; }
                    None => { let _ = socket.close().await; break; }
                },
                msg = socket.recv() => match msg {
                    Some(Ok(AxMsg::Text(t))) => { let _ = from_tx.send(serde_json::from_str(t.as_str()).unwrap()); }
                    Some(Ok(_)) => {}
                    _ => break,
                },
            }
        }
    })
}

async fn attach_route(
    State(f): State<Fake>,
    Path((_f, _c, _a, session)): Path<(String, String, String, String)>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |mut socket: WebSocket| async move {
        let (to_tx, mut to_rx) = mpsc::unbounded_channel::<AxMsg>();
        let (from_tx, from_rx) = mpsc::unbounded_channel::<AxMsg>();
        let _ = f.attaches.send(AttachConn {
            headers,
            session,
            to_sidecar: to_tx,
            from_sidecar: from_rx,
        });
        loop {
            tokio::select! {
                out = to_rx.recv() => match out {
                    Some(m) => { if socket.send(m).await.is_err() { break; } }
                    None => { let _ = socket.close().await; break; }
                },
                msg = socket.recv() => match msg {
                    Some(Ok(m)) => { let _ = from_tx.send(m); }
                    _ => break,
                },
            }
        }
    })
}

fn fake_router(f: Fake) -> Router {
    Router::new()
        .route("/v1/agents/{f}/{c}/{a}/link", get(link_route))
        .route("/v1/agents/{f}/{c}/{a}/link/attach/{s}", get(attach_route))
        .with_state(f)
}

async fn fake_daemon(listener: tokio::net::TcpListener) -> (mpsc::UnboundedReceiver<Conn>, mpsc::UnboundedReceiver<AttachConn>) {
    let (links, links_rx) = mpsc::unbounded_channel();
    let (attaches, attaches_rx) = mpsc::unbounded_channel();
    tokio::spawn(axum::serve(listener, fake_router(Fake { links, attaches })).into_future());
    (links_rx, attaches_rx)
}

/// A reader of two paths: `f` is five bytes, anything else is absent;
/// the other reads are never made here.
struct TwoFiles;

impl WorkspaceReader for TwoFiles {
    fn diff(&self, _: &AgentId, _: &str) -> Result<WorkspaceDiff, WorkspaceError> {
        Err(WorkspaceError::NoSuchPath)
    }
    fn read_file(&self, _: &AgentId, path: &str) -> Result<Vec<u8>, WorkspaceError> {
        if path == "f" { Ok(b"hello".to_vec()) } else { Err(WorkspaceError::NoSuchPath) }
    }
    fn list_dir(&self, _: &AgentId, _: &str) -> Result<WorkspaceTree, WorkspaceError> {
        Err(WorkspaceError::NoSuchPath)
    }
    fn version(&self, _: &AgentId, _: &str) -> Result<WorkspaceVersion, WorkspaceError> {
        Err(WorkspaceError::NoSuchPath)
    }
}

struct Side {
    runner: Arc<FakeRunner>,
    control: mpsc::UnboundedReceiver<Control>,
    status: watch::Sender<LinkStatus>,
}

fn start_sidecar_link(daemon_url: &str) -> Side {
    let dir = tempfile::tempdir().unwrap();
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().unwrap();
    std::fs::write(dir.path().join("ca.crt"), ca.self_signed(&key).unwrap().pem()).unwrap();
    let tls = client_config(&dir.path().join("ca.crt")).unwrap();
    let runner = Arc::new(FakeRunner::default());
    let (control_tx, control) = mpsc::unbounded_channel();
    let (status, status_rx) = watch::channel(LinkStatus {
        status: AgentStatus::default(),
        pid: None,
        hook_failures: 0,
    });
    let deps = Arc::new(LinkDeps {
        id: "f/c/a".parse().unwrap(),
        token: TOKEN.into(),
        daemon_url: daemon_url.into(),
        tls,
        runner: runner.clone(),
        workspace: Arc::new(TwoFiles),
        control: control_tx,
        status: status_rx,
    });
    tokio::spawn(run(deps));
    Side { runner, control, status }
}

async fn reply(conn: &mut Conn) -> (u64, LinkResult) {
    loop {
        match conn.from_sidecar.recv().await.unwrap() {
            SidecarFrame::Reply(r) => return (r.id, r.result),
            SidecarFrame::Status(_) => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_link_connects_answers_requests_and_reconnects() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (mut links, mut attaches) = fake_daemon(listener).await;
    let mut side = start_sidecar_link(&url);

    let mut conn = tokio::time::timeout(Duration::from_secs(5), links.recv()).await.unwrap().unwrap();
    assert_eq!(conn.headers["authorization"], format!("Bearer {TOKEN}"));
    assert_eq!(conn.headers["balerix-link-protocol"], "1");
    // the current status comes first
    match conn.from_sidecar.recv().await.unwrap() {
        SidecarFrame::Status(s) => assert_eq!(s.status.phase, AgentPhase::Pending),
        other => panic!("{other:?}"),
    }

    conn.to_sidecar.send(LinkRequest { id: 1, op: LinkOp::SendText { text: "hi".into(), submit: true } }).unwrap();
    assert_eq!(reply(&mut conn).await, (1, LinkResult::Ok));
    assert!(side.runner.calls().iter().any(|c| c == "send_text f/c/a \"hi\" submit=true"));

    conn.to_sidecar.send(LinkRequest { id: 2, op: LinkOp::Stop }).unwrap();
    assert_eq!(reply(&mut conn).await, (2, LinkResult::Ok));
    assert_eq!(side.control.recv().await.unwrap(), Control::Stop);
    conn.to_sidecar.send(LinkRequest { id: 3, op: LinkOp::Restart }).unwrap();
    assert_eq!(reply(&mut conn).await, (3, LinkResult::Ok));
    assert_eq!(side.control.recv().await.unwrap(), Control::Restart);

    conn.to_sidecar.send(LinkRequest { id: 4, op: LinkOp::WorkspaceFile { path: "missing".into() } }).unwrap();
    let (id, result) = reply(&mut conn).await;
    assert_eq!(id, 4);
    assert!(matches!(result, LinkResult::Failed { failure } if failure.reason == FailureKind::NoSuchPath));
    conn.to_sidecar.send(LinkRequest { id: 5, op: LinkOp::WorkspaceFile { path: "f".into() } }).unwrap();
    assert_eq!(reply(&mut conn).await, (5, LinkResult::File { bytes: b"hello".to_vec() }));

    // attach: the second socket comes up with the token before the reply
    conn.to_sidecar.send(LinkRequest { id: 6, op: LinkOp::Attach { session: "s1".into() } }).unwrap();
    let mut att = tokio::time::timeout(Duration::from_secs(5), attaches.recv()).await.unwrap().unwrap();
    assert_eq!(att.session, "s1");
    assert_eq!(att.headers["authorization"], format!("Bearer {TOKEN}"));
    assert_eq!(reply(&mut conn).await, (6, LinkResult::Ok));
    // FakeRunner's attach is an echo: bytes written come back on the reader
    att.to_sidecar.send(AxMsg::Binary(b"abc".to_vec().into())).unwrap();
    let echoed = tokio::time::timeout(Duration::from_secs(5), att.from_sidecar.recv()).await.unwrap().unwrap();
    assert_eq!(echoed, AxMsg::Binary(b"abc".to_vec().into()));
    att.to_sidecar.send(AxMsg::Text(r#"{"resize":{"cols":100,"rows":30}}"#.into())).unwrap();
    let start = Instant::now();
    while !side.runner.resizes().iter().any(|(a, c, r)| a == "f/c/a" && *c == 100 && *r == 30) {
        assert!(start.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // the window dies: the sidecar closes the attach socket
    assert!(side.runner.close_attach(&"f/c/a".parse().unwrap()));
    let start = Instant::now();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), att.from_sidecar.recv()).await.unwrap() {
            Some(AxMsg::Close(_)) | None => break,
            Some(_) => {}
        }
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    // a status change is a frame
    side.status
        .send(LinkStatus {
            status: AgentStatus { phase: AgentPhase::Ready, ..AgentStatus::default() },
            pid: Some(7),
            hook_failures: 1,
        })
        .unwrap();
    loop {
        match conn.from_sidecar.recv().await.unwrap() {
            SidecarFrame::Status(s) if s.status.phase == AgentPhase::Ready => {
                assert_eq!((s.pid, s.hook_failures), (Some(7), 1));
                break;
            }
            _ => {}
        }
    }

    // the Daemon drops the link: the sidecar is back within the back-off
    drop(conn);
    let again = tokio::time::timeout(Duration::from_secs(5), links.recv()).await.unwrap().unwrap();
    assert_eq!(again.headers["authorization"], format!("Bearer {TOKEN}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_that_is_not_up_yet_is_reached_when_it_comes_up() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let _side = start_sidecar_link(&format!("http://{addr}"));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let (mut links, _) = fake_daemon(listener).await;
    let conn = tokio::time::timeout(Duration::from_secs(10), links.recv()).await.unwrap().unwrap();
    assert_eq!(conn.headers["balerix-link-protocol"], "1");
}
```

`FakeRunner::calls()` records `send_text <id> "<text>" submit=<b>` (core report, fakes section); `resizes()` returns `Vec<(String, u16, u16)>`; `close_attach(&AgentId) -> bool`.

- [ ] **Step 2: Run to see it fail**

Run: `CARGO_TARGET_DIR=agent/target mise x -- cargo test --manifest-path agent/Cargo.toml --test link_it`
Expected: compile errors (`balerix_agent::link` missing).

- [ ] **Step 3: `link.rs`**

```rust
//! The sidecar's end of the link (Spec O §7.2): one outbound WebSocket to
//! the Daemon, reconnecting with back-off. Requests are dispatched to the
//! runner, the workspace reader or the sidecar loop (`stop`, `restart`) and
//! answered by id; a `status` frame goes out on connect and on every
//! change the sidecar loop publishes.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use balerix_api::{
    FailureKind, LINK_PROTOCOL, LINK_PROTOCOL_HEADER, LinkFailure, LinkOp, LinkReply, LinkRequest,
    LinkResult, LinkStatus, SidecarFrame,
};
use balerix_core::{AgentId, AgentRunner, WorkspaceError, WorkspaceReader};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config};

pub const RECONNECT_MIN: Duration = Duration::from_secs(1);
pub const RECONNECT_MAX: Duration = Duration::from_secs(30);

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// What the Daemon may change about the agent's desired state (plugins
/// spec §16.4 over the link); the sidecar loop moves its stopped set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Stop,
    Restart,
}

pub struct LinkDeps {
    pub id: AgentId,
    pub token: String,
    /// `https://host:port` (or `http://` under `--allow-plain-http`).
    pub daemon_url: String,
    pub tls: Arc<rustls::ClientConfig>,
    pub runner: Arc<dyn AgentRunner>,
    pub workspace: Arc<dyn WorkspaceReader>,
    pub control: mpsc::UnboundedSender<Control>,
    pub status: watch::Receiver<LinkStatus>,
}

/// `https://` → `wss://`, `http://` → `ws://`, plus `path`.
pub fn ws_url(daemon_url: &str, path: &str) -> String {
    let base = daemon_url.trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!("{base}{path}")
}

/// A socket to the Daemon at `path`, with the token and the protocol.
pub async fn connect(deps: &LinkDeps, path: &str) -> Result<Ws> {
    let mut req = ws_url(&deps.daemon_url, path).into_client_request()?;
    req.headers_mut()
        .insert(AUTHORIZATION, format!("Bearer {}", deps.token).parse()?);
    req.headers_mut()
        .insert(LINK_PROTOCOL_HEADER, LINK_PROTOCOL.to_string().parse()?);
    let (ws, _) = connect_async_tls_with_config(
        req,
        None,
        false,
        Some(Connector::Rustls(deps.tls.clone())),
    )
    .await?;
    Ok(ws)
}

/// For the life of the sidecar: connect, serve the session, reconnect.
pub async fn run(deps: Arc<LinkDeps>) {
    let mut backoff = RECONNECT_MIN;
    loop {
        match connect(&deps, &format!("/v1/agents/{}/link", deps.id)).await {
            Ok(ws) => {
                tracing::info!("link up");
                backoff = RECONNECT_MIN;
                session(deps.clone(), ws).await;
                tracing::warn!("link closed");
            }
            Err(e) => tracing::warn!("link: cannot connect: {e}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

async fn send(ws: &mut Ws, frame: &SidecarFrame) -> Result<()> {
    ws.send(Message::Text(serde_json::to_string(frame)?.into()))
        .await?;
    Ok(())
}

async fn session(deps: Arc<LinkDeps>, mut ws: Ws) {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SidecarFrame>();
    let mut status = deps.status.clone();
    // the current status first: a Daemon that restarted sees the agent
    // at once
    let current = status.borrow_and_update().clone();
    if send(&mut ws, &SidecarFrame::Status(current)).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            changed = status.changed() => {
                if changed.is_err() {
                    return;
                }
                let frame = SidecarFrame::Status(status.borrow_and_update().clone());
                if send(&mut ws, &frame).await.is_err() {
                    return;
                }
            }
            Some(frame) = out_rx.recv() => {
                if send(&mut ws, &frame).await.is_err() {
                    return;
                }
            }
            msg = ws.next() => match msg {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<LinkRequest>(text.as_str()) {
                    Ok(req) => {
                        // each request on its own task: a paced send_keys
                        // must not hold up a status frame or another request
                        let deps = deps.clone();
                        let out = out_tx.clone();
                        tokio::spawn(async move {
                            let result = dispatch(&deps, req.op).await;
                            let _ = out.send(SidecarFrame::Reply(LinkReply { id: req.id, result }));
                        });
                    }
                    Err(e) => tracing::warn!("link: not a request: {e}"),
                },
                Some(Ok(Message::Ping(p))) => {
                    if ws.send(Message::Pong(p)).await.is_err() {
                        return;
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

fn runner_failed(message: String) -> LinkResult {
    LinkResult::Failed {
        failure: LinkFailure {
            reason: FailureKind::Runner,
            message,
        },
    }
}

/// `WorkspaceError` by variant, with its text.
pub fn workspace_failure(e: WorkspaceError) -> LinkFailure {
    use WorkspaceError as W;
    let reason = match &e {
        W::Missing(_) => FailureKind::Missing,
        W::NoSuchPath => FailureKind::NoSuchPath,
        W::InvalidPath(_) => FailureKind::InvalidPath,
        W::NotAFile => FailureKind::NotAFile,
        W::NotADirectory => FailureKind::NotADirectory,
        W::TooLarge { limit } => FailureKind::TooLarge { limit: *limit },
        W::Tool { .. } => FailureKind::Tool,
        W::Filter { .. } => FailureKind::Filter,
        W::Io { .. } => FailureKind::Io,
    };
    LinkFailure {
        reason,
        message: e.to_string(),
    }
}

/// A port call off the runtime. `Err(String)` is a task failure.
async fn blocking<T, E>(f: impl FnOnce() -> Result<T, E> + Send + 'static) -> Result<Result<T, E>, String>
where
    T: Send + 'static,
    E: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("task failed: {e}"))
}

fn workspace_result<T>(r: Result<Result<T, WorkspaceError>, String>, ok: impl FnOnce(T) -> LinkResult) -> LinkResult {
    match r {
        Ok(Ok(v)) => ok(v),
        Ok(Err(e)) => LinkResult::Failed {
            failure: workspace_failure(e),
        },
        Err(m) => runner_failed(m),
    }
}

pub async fn dispatch(deps: &Arc<LinkDeps>, op: LinkOp) -> LinkResult {
    let id = deps.id.clone();
    match op {
        LinkOp::SendText { text, submit } => {
            let r = deps.runner.clone();
            match blocking(move || r.send_text(&id, &text, submit)).await {
                Ok(Ok(())) => LinkResult::Ok,
                Ok(Err(e)) => runner_failed(e.to_string()),
                Err(m) => runner_failed(m),
            }
        }
        LinkOp::SendKeys { steps, delay_ms } => {
            let r = deps.runner.clone();
            let delay = Duration::from_millis(delay_ms);
            match blocking(move || r.send_keys(&id, &steps, delay)).await {
                Ok(Ok(())) => LinkResult::Ok,
                Ok(Err(e)) => runner_failed(e.to_string()),
                Err(m) => runner_failed(m),
            }
        }
        LinkOp::Stop => match deps.control.send(Control::Stop) {
            Ok(()) => LinkResult::Ok,
            Err(_) => runner_failed("the sidecar loop is gone".into()),
        },
        LinkOp::Restart => match deps.control.send(Control::Restart) {
            Ok(()) => LinkResult::Ok,
            Err(_) => runner_failed("the sidecar loop is gone".into()),
        },
        LinkOp::Attach { session } => crate::attach::open(deps, &session).await,
        LinkOp::WorkspaceDiff { base_ref } => {
            let w = deps.workspace.clone();
            workspace_result(blocking(move || w.diff(&id, &base_ref)).await, |diff| LinkResult::Diff { diff })
        }
        LinkOp::WorkspaceFile { path } => {
            let w = deps.workspace.clone();
            workspace_result(blocking(move || w.read_file(&id, &path)).await, |bytes| LinkResult::File { bytes })
        }
        LinkOp::WorkspaceTree { path } => {
            let w = deps.workspace.clone();
            workspace_result(blocking(move || w.list_dir(&id, &path)).await, |tree| LinkResult::Tree { tree })
        }
        LinkOp::WorkspaceVersion { base_ref } => {
            let w = deps.workspace.clone();
            workspace_result(blocking(move || w.version(&id, &base_ref)).await, |version| LinkResult::Version { version })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_swaps_the_scheme_and_appends_the_path() {
        assert_eq!(ws_url("https://d:7643/", "/v1/x"), "wss://d:7643/v1/x");
        assert_eq!(ws_url("http://127.0.0.1:1", "/v1/x"), "ws://127.0.0.1:1/v1/x");
    }
}
```

`WorkspaceError`'s variants are those the core report lists (`ports.rs:145-175`); adjust the match if a field name differs.

- [ ] **Step 4: `attach.rs`**

```rust
//! `attach` on the sidecar's side (Spec O §7.2): the runner's PTY on the
//! agent's window, carried to the Daemon over a second socket. The
//! protocol is the plugin attach protocol with the roles reversed: binary
//! frames are terminal bytes both ways; the Daemon's one text frame is a
//! resize; the Daemon closes when the viewer is done, and the stream's drop
//! then ends the grouped tmux session (`TmuxAttach`).

use std::io::{Read, Write};
use std::sync::Arc;

use balerix_api::{FailureKind, LinkFailure, LinkResult, ResizeFrame, TextFrame};
use balerix_core::PtyStream;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::link::{LinkDeps, Ws, connect};

const READ_CHUNK: usize = 8192;

fn failed(message: String) -> LinkResult {
    LinkResult::Failed {
        failure: LinkFailure {
            reason: FailureKind::Runner,
            message,
        },
    }
}

/// Attaches through the runner, opens the session's socket, answers `Ok`
/// once it is up, and leaves the bridge running.
pub async fn open(deps: &Arc<LinkDeps>, session: &str) -> LinkResult {
    let (runner, id) = (deps.runner.clone(), deps.id.clone());
    let stream = match tokio::task::spawn_blocking(move || runner.attach(&id)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return failed(e.to_string()),
        Err(e) => return failed(format!("task failed: {e}")),
    };
    let path = format!("/v1/agents/{}/link/attach/{session}", deps.id);
    let ws = match connect(deps, &path).await {
        Ok(ws) => ws,
        Err(e) => {
            let _ = tokio::task::spawn_blocking(move || drop(stream)).await;
            return failed(format!("attach socket: {e}"));
        }
    };
    tokio::spawn(bridge(ws, stream));
    LinkResult::Ok
}

/// Mirrors `balerix-server`'s `attach::bridge` with the roles reversed.
pub async fn bridge(mut ws: Ws, stream: Box<dyn PtyStream>) {
    let (reader, mut writer) = match (stream.reader(), stream.writer()) {
        (Ok(r), Ok(w)) => (r, w),
        _ => {
            let _ = ws.close(None).await;
            let _ = tokio::task::spawn_blocking(move || drop(stream)).await;
            return;
        }
    };
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
    let pump = tokio::task::spawn_blocking(move || {
        let mut reader: Box<dyn Read + Send> = reader;
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    loop {
        tokio::select! {
            chunk = rx.recv() => match chunk {
                Some(bytes) => {
                    if ws.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                None => {
                    // the window closed
                    let _ = ws.close(None).await;
                    break;
                }
            },
            msg = ws.next() => match msg {
                Some(Ok(Message::Binary(bytes))) => {
                    match tokio::task::spawn_blocking(move || {
                        let result = writer.write_all(&bytes).and_then(|()| writer.flush());
                        (writer, result)
                    })
                    .await
                    {
                        Ok((w, Ok(()))) => writer = w,
                        _ => break,
                    }
                }
                Some(Ok(Message::Text(text))) => match ResizeFrame::parse(text.as_str()) {
                    TextFrame::Resize(frame) => {
                        if let Err(e) = stream.resize(frame.resize.cols, frame.resize.rows) {
                            tracing::debug!("attach resize failed: {e}");
                        }
                    }
                    TextFrame::ZeroSized => {}
                    TextFrame::Malformed => break,
                },
                Some(Ok(Message::Ping(p))) => {
                    let _ = ws.send(Message::Pong(p)).await;
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    drop(pump);
    // the tmux stream's drop kills its client and waits for it: off the
    // runtime, and awaited so the grouped session is really gone
    let _ = tokio::task::spawn_blocking(move || drop(stream)).await;
}
```

Add `pub mod attach;` and `pub mod link;` to `agent/src/lib.rs`.

- [ ] **Step 5: Run the tests**

Run: `mise run agent`
Expected: `link_it` 2 passed (the reconnect test takes a few seconds), everything else green.

- [ ] **Step 6: Commit**

```bash
git add agent
git commit -m "feat(agent): the sidecar's link: requests answered by id, status frames, reconnect with back-off, attach over a second socket (Spec O §7.2)"
```

---

### Task 13: The sidecar, and the two-process tests

**Files:**
- Create: `agent/src/bundle.rs`, `agent/src/sidecar.rs`, `agent/tests/support/fake_daemon.rs`, `agent/tests/sidecar_it.rs`
- Modify: `agent/src/lib.rs`, `agent/src/main.rs` (the `Sidecar` arm), `agent/tests/link_it.rs` (use the shared fake Daemon)

**Interfaces:**
- Consumes: everything above. `Runtime` (`Materializer` + `WorkspaceReader`), `TmuxRunner::at_socket`, `StateLayout::pod`, `PodLayout::{tmux_socket, start_marker, ready_marker}`, `sandbox_self_test` (Tasks 3–5); `hooks::{Hooks, router}`, `tls`, `link::{LinkDeps, Control, run}` (Tasks 11–12); `balerix_core::reconcile::{reconcile_pass, ReconcileContext, agent_ready, set_desired}`, `ReconcilePolicy`, `FleetStatus`, `Keep`, `CrewTools`, `HookTarget`, `Clock`, `Timestamp`.
- Produces: `balerix-agent sidecar` (Spec O §6.2 and §6.3 end to end):
  1. loads the bundle (`cannot read the agent bundle at <path>: …`), refuses an `http://` Daemon without `--allow-plain-http` (`daemon_url must be https:// (Spec O §10.3)`), loads the authority;
  2. binds the hook listener on `127.0.0.1:<hook_port>` (its address is the agent's `HookTarget.url`, the token its secret);
  3. `ensure_crew` (the slice check) and `materialize` through `Runtime`, then `sandbox_self_test`; any failure is the termination message (`SandboxUnavailable: …` for a node without Landlock) and exit 1;
  4. writes `<run>/started` with `<fleet>/<crew>` and waits up to 120 s for the tmux server (`observe` shows the crew);
  5. runs the planner loop: a pass, then sleeps until the next restart is due (1 s floor, 30 s cap), or a `stop`/`restart` from the link moves the stopped set and passes again, or a hook event arrives (`SessionStart` → `agent_ready`); every change of the agent's status is published as a `status` frame with the pane's pid and the hook-failure count; `<run>/ready` exists exactly while the phase is `Ready`; SIGTERM ends the sidecar.
  - `balerix_agent::bundle::{Loaded, load}`: `Loaded { bundle: AgentBundle, id: AgentId, fleet: Fleet, agent: ResolvedAgent }`.
  - `agent/tests/support/fake_daemon.rs`: `FakeDaemon::start(addr: Option<SocketAddr>) -> FakeDaemon` (routes: the link, the attach socket, the events route recording names and answering `{}`), fields `addr`, `links: Receiver<Conn>`, `attaches: Receiver<AttachConn>`, `events: Receiver<String>`, method `stop(self)`.

- [ ] **Step 1: `bundle.rs`**

```rust
//! The operator's bundle, turned into what the planner wants: one fleet
//! with one crew and one agent (Spec O §5.4, §6.2).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow, ensure};
use balerix_api::{AgentBundle, CrewSpec, FleetSpec};
use balerix_core::{AgentId, Fleet, ResolvedAgent};

pub struct Loaded {
    pub bundle: AgentBundle,
    pub id: AgentId,
    pub fleet: Fleet,
    pub agent: ResolvedAgent,
}

pub fn load(path: &Path) -> Result<Loaded> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the agent bundle at {}", path.display()))?;
    let bundle: AgentBundle = serde_json::from_str(&text)
        .with_context(|| format!("{}: not an agent bundle", path.display()))?;
    let id: AgentId = bundle
        .agent
        .parse()
        .map_err(|e| anyhow!("{}: agent: {e}", path.display()))?;
    ensure!(
        bundle.token.len() >= 32,
        "{}: the token is at least 32 characters",
        path.display()
    );
    let spec = FleetSpec {
        name: id.fleet.to_string(),
        tools: BTreeMap::new(),
        crews: BTreeMap::from([(
            id.crew.to_string(),
            CrewSpec {
                repo: bundle.repo.clone(),
                git_ref: bundle.git_ref.clone(),
                git: bundle.git.clone(),
                tools: BTreeMap::new(),
                agents: BTreeMap::from([(id.agent.to_string(), bundle.settings.clone())]),
            },
        )]),
    };
    let fleet = Fleet::try_from(spec).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    let agent = ResolvedAgent::from_fleet(&fleet)
        .into_iter()
        .find(|a| a.id == id)
        .context("the bundle's agent is not in its own fleet")?;
    Ok(Loaded {
        bundle,
        id,
        fleet,
        agent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bundle_becomes_one_fleet_with_one_agent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "agent": "payments/backend/alice",
                "repo": "acme/payments-api",
                "git_ref": "main",
                "settings": { "claude": { "settings": { "model": "sonnet" } } },
                "daemon_url": "https://d:7643",
                "token": "0123456789abcdef0123456789abcdef"
            })
            .to_string(),
        )
        .unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.id.to_string(), "payments/backend/alice");
        assert_eq!(loaded.fleet.name.as_str(), "payments");
        assert_eq!(loaded.agent.git_ref, "main");
        assert_eq!(loaded.agent.branch(), "balerix/payments/backend/alice");
    }

    #[test]
    fn errors_name_the_file() {
        let e = load(std::path::Path::new("/nonexistent/agent.json")).unwrap_err();
        assert!(
            format!("{e:#}").starts_with("cannot read the agent bundle at /nonexistent/agent.json"),
            "{e:#}"
        );
    }
}
```

Check `ResolvedAgent::branch()`'s format in `balerix-core/src/agent.rs:113` and set the expected string to what it renders.

- [ ] **Step 2: `sidecar.rs`**

```rust
//! The sidecar (Spec O §6.2, §6.3): a one-agent daemon. The same planner,
//! materializer and runner the daemon uses on one machine, over a pod
//! layout and a shared tmux socket; Claude's hooks forwarded; one outbound
//! link to the Daemon carrying `send_text`, `send_keys`, `stop`,
//! `restart`, `attach` and the workspace reads.

use std::collections::{BTreeMap, BTreeSet};
use std::future::IntoFuture;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use balerix_api::{AgentStatus, FleetStatus, Keep, LinkStatus, Timestamp};
use balerix_core::reconcile::{ReconcileContext, agent_ready, reconcile_pass, set_desired};
use balerix_core::{
    AgentId, AgentRunner, Clock, CrewTools, HookTarget, Materializer, ProcessState,
    ReconcilePolicy, WorkspaceReader,
};
use balerix_runtime::layout::{PodLayout, PodMounts};
use balerix_runtime::sandbox::sandbox_self_test;
use balerix_runtime::{Runtime, StateLayout, TmuxRunner, ToolPaths};
use tokio::sync::{mpsc, watch};

use crate::cli::SidecarArgs;
use crate::hooks::{self, Hooks};
use crate::link::{self, Control, LinkDeps};
use crate::{bundle, run, tls};

/// How long the `agent` container may take to bring the tmux server up
/// after the start marker (image pull is before this; it is only the
/// container start and `new-session`).
pub const SERVER_WAIT: Duration = Duration::from_secs(120);
/// The planner's cadence when nothing is due (the daemon's `RESYNC`).
pub const RESYNC: Duration = Duration::from_secs(30);
const MIN_TICK: Duration = Duration::from_secs(1);
const READY_EVENT: &str = "SessionStart";

struct WallClock;

impl Clock for WallClock {
    fn now(&self) -> Timestamp {
        Timestamp(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        )
    }
}

pub async fn main(args: SidecarArgs) -> Result<()> {
    let loaded = bundle::load(&args.bundle)?;
    let id = loaded.id.clone();
    let daemon_url = loaded.bundle.daemon_url.clone();
    if daemon_url.starts_with("http://") && !args.allow_plain_http {
        bail!("daemon_url must be https:// (Spec O §10.3)");
    }
    let tls = tls::client_config(&args.ca)?;
    let layout = StateLayout::pod(
        PodMounts {
            agent: args.agent_dir.clone(),
            shared: args.shared_dir.clone(),
            run: args.run_dir.clone(),
        },
        &id,
    );
    let pod: PodLayout = layout.pod_layout().cloned().context("pod layout")?;
    let balerix = match &args.balerix {
        Some(p) => p.clone(),
        None => run::on_path("balerix").context("balerix is not on PATH; pass --balerix")?,
    };
    let tools = ToolPaths::discover_in(&std::env::var_os("PATH").unwrap_or_default(), &balerix)
        .map_err(|e| anyhow!("{e} (the sidecar needs git, gh, mise, nono and tmux on PATH)"))?;
    let runtime = Arc::new(Runtime::new(layout.clone(), tools.clone()));
    let runner = Arc::new(TmuxRunner::at_socket(tools.tmux.clone(), pod.tmux_socket()));

    // §7.1: the hook listener first; its port goes into the profile's
    // `open_port` and its URL into `settings.json`, through `HookTarget`
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.hook_port))
        .await
        .with_context(|| format!("cannot bind 127.0.0.1:{}", args.hook_port))?;
    let hook_target = HookTarget {
        url: format!("http://{}", listener.local_addr()?),
        secret: loaded.bundle.token.clone(),
    };

    // §6.2 step 1: the files, as the daemon makes them
    {
        let (rt, agent, creds, hooks) = (
            runtime.clone(),
            loaded.agent.clone(),
            loaded.bundle.credentials.clone(),
            hook_target.clone(),
        );
        tokio::task::spawn_blocking(move || -> Result<()> {
            let empty = BTreeMap::new();
            rt.ensure_crew(
                &agent.id.crew_ref(),
                &agent.repo,
                &agent.git_ref,
                &agent.git,
                &creds,
                CrewTools {
                    fleet: &empty,
                    crew: &empty,
                },
            )?;
            rt.materialize(&agent, &creds, &hooks)?;
            Ok(())
        })
        .await??;
    }
    // §6.2 step 2: the sandbox self-test; its failure is the termination
    // message (`SandboxUnavailable: …` where Landlock is denied)
    {
        let (tools, paths) = (tools.clone(), layout.agent(&id));
        tokio::task::spawn_blocking(move || sandbox_self_test(&tools, &paths))
            .await?
            .map_err(|e| anyhow!("{e}"))?;
    }
    // §6.2 step 3: the marker, then the server the agent container starts
    std::fs::create_dir_all(&args.run_dir)?;
    std::fs::write(pod.start_marker(), format!("{}\n", id.crew_ref()))?;
    wait_for_server(&runner, &id).await?;
    tracing::info!(agent = %id, "materialised; tmux server up");

    // the long-lived parts: hooks, the link, the planner
    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<String>();
    let failures = Arc::new(AtomicU64::new(0));
    let hooks_state = Arc::new(Hooks {
        id: id.clone(),
        token: loaded.bundle.token.clone(),
        daemon_url: daemon_url.clone(),
        http: tls::http_client(&tls, Duration::from_secs(10))?,
        failures: failures.clone(),
        events: events_tx,
    });
    tokio::spawn(axum::serve(listener, hooks::router(hooks_state)).into_future());
    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Control>();
    let (status_tx, status_rx) = watch::channel(LinkStatus {
        status: AgentStatus::default(),
        pid: None,
        hook_failures: 0,
    });
    tokio::spawn(link::run(Arc::new(LinkDeps {
        id: id.clone(),
        token: loaded.bundle.token.clone(),
        daemon_url,
        tls,
        runner: runner.clone() as Arc<dyn AgentRunner>,
        workspace: runtime.clone() as Arc<dyn WorkspaceReader>,
        control: control_tx,
        status: status_rx,
    })));

    let fleet = Arc::new(loaded.fleet);
    let creds = Arc::new(loaded.bundle.credentials.clone());
    let policy = Arc::new(ReconcilePolicy::default());
    let clock = Arc::new(WallClock);
    let mut status = FleetStatus::default();
    set_desired(&mut status, 1);
    let mut stopped: BTreeSet<AgentId> = BTreeSet::new();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        let (new_status, pid, clean) = {
            let (fleet, creds, policy, clock, runtime, runner, hooks, stopped, id) = (
                fleet.clone(),
                creds.clone(),
                policy.clone(),
                clock.clone(),
                runtime.clone(),
                runner.clone(),
                hook_target.clone(),
                stopped.clone(),
                id.clone(),
            );
            let mut status = std::mem::take(&mut status);
            tokio::task::spawn_blocking(move || {
                let hooks = |_: &AgentId| hooks.clone();
                let ctx = ReconcileContext {
                    fleet: &id.fleet,
                    desired: Some(&fleet),
                    keep: Keep::default(),
                    stopped: &stopped,
                    materializer: runtime.as_ref(),
                    runner: runner.as_ref(),
                    creds: &creds,
                    hooks: &hooks,
                    policy: &policy,
                    clock: clock.as_ref(),
                };
                let clean = match reconcile_pass(&mut status, &ctx) {
                    Ok((plan, report)) => {
                        for (step, err) in &report.failures {
                            tracing::warn!(step = %step, "step failed: {err}");
                        }
                        tracing::debug!(steps = plan.len(), failed = report.failures.len(), "pass");
                        report.all_ok()
                    }
                    Err(e) => {
                        tracing::error!("pass failed: {e}");
                        false
                    }
                };
                let pid = match runner.observe(&id.fleet) {
                    Ok(observed) => match observed.get(&id) {
                        Some(ProcessState::Running { pid }) => Some(*pid),
                        _ => None,
                    },
                    Err(_) => None,
                };
                (status, pid, clean)
            })
            .await?
        };
        status = new_status;
        publish(&status_tx, &status, &id, pid, &failures);
        ready_marker(&pod, &status, &id)?;

        let wake = tokio::time::sleep(next_deadline(&status, clean, clock.now()));
        tokio::pin!(wake);
        loop {
            tokio::select! {
                () = &mut wake => break,
                Some(c) = control_rx.recv() => {
                    match c {
                        Control::Stop => {
                            stopped.insert(id.clone());
                        }
                        Control::Restart => {
                            stopped.remove(&id);
                        }
                    }
                    break;
                }
                Some(name) = events_rx.recv() => {
                    if name == READY_EVENT {
                        tracing::info!(agent = %id, "agent ready ({READY_EVENT} received)");
                        agent_ready(&mut status, &id, clock.now());
                        publish(&status_tx, &status, &id, pid, &failures);
                        ready_marker(&pod, &status, &id)?;
                    }
                }
                _ = term.recv() => {
                    tracing::info!("SIGTERM; the agent container ends the tree");
                    return Ok(());
                }
            }
        }
    }
}

/// `observe` lists the crew's session once the agent container's server
/// is up with the anchor window.
async fn wait_for_server(runner: &Arc<TmuxRunner>, id: &AgentId) -> Result<()> {
    let deadline = tokio::time::Instant::now() + SERVER_WAIT;
    loop {
        let (r, fleet, crew) = (runner.clone(), id.fleet.clone(), id.crew.clone());
        let up = tokio::task::spawn_blocking(move || r.observe(&fleet))
            .await?
            .map(|o| o.crews.contains_key(&crew))
            .unwrap_or(false);
        if up {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "the agent container did not start the tmux server within {} s",
                SERVER_WAIT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The agent's status as a frame, sent only when it changed.
fn publish(
    tx: &watch::Sender<LinkStatus>,
    status: &FleetStatus,
    id: &AgentId,
    pid: Option<u32>,
    failures: &AtomicU64,
) {
    let frame = LinkStatus {
        status: status
            .agents
            .get(&id.to_string())
            .cloned()
            .unwrap_or_default(),
        pid,
        hook_failures: failures.load(Ordering::Relaxed),
    };
    tx.send_if_modified(|current| {
        if *current == frame {
            false
        } else {
            *current = frame;
            true
        }
    });
}

/// `<run>/ready` exists exactly while the agent is `Ready` (the pod's
/// exec readiness probe).
fn ready_marker(pod: &PodLayout, status: &FleetStatus, id: &AgentId) -> Result<()> {
    let ready = status
        .agents
        .get(&id.to_string())
        .is_some_and(|a| a.phase == balerix_api::AgentPhase::Ready);
    let marker = pod.ready_marker();
    if ready {
        if !marker.exists() {
            std::fs::write(&marker, "ready\n")?;
        }
    } else if let Err(e) = std::fs::remove_file(&marker)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(e.into());
    }
    Ok(())
}

/// The daemon actor's `deadline`: the next restart if one is due, the
/// resync otherwise; a dirty pass waits the resync out.
fn next_deadline(status: &FleetStatus, clean: bool, now: Timestamp) -> Duration {
    if !clean {
        return RESYNC;
    }
    match status.agents.values().filter_map(|a| a.next_restart_at).min() {
        Some(due) => Duration::from_secs(due.0.saturating_sub(now.0))
            .max(MIN_TICK)
            .min(RESYNC),
        None => RESYNC,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::AgentPhase;

    #[test]
    fn the_deadline_follows_the_next_restart_within_the_floor_and_the_cap() {
        let mut s = FleetStatus::default();
        assert_eq!(next_deadline(&s, true, Timestamp(100)), RESYNC);
        assert_eq!(next_deadline(&s, false, Timestamp(100)), RESYNC);
        s.entry("f/c/a").next_restart_at = Some(Timestamp(105));
        assert_eq!(next_deadline(&s, true, Timestamp(100)), Duration::from_secs(5));
        s.entry("f/c/a").next_restart_at = Some(Timestamp(100));
        assert_eq!(next_deadline(&s, true, Timestamp(100)), MIN_TICK);
        s.entry("f/c/a").next_restart_at = Some(Timestamp(10_000));
        assert_eq!(next_deadline(&s, true, Timestamp(100)), RESYNC);
        s.entry("f/c/a").phase = AgentPhase::Ready;
    }
}
```

`FleetStatus::entry(&mut self, id: &str) -> &mut AgentStatus` exists (`status.rs:151`); `AgentStatus: Default`; `PodLayout: Clone`. `agent/src/lib.rs` gains `pub mod bundle;` and `pub mod sidecar;`. `agent/src/main.rs`'s `Sidecar` arm becomes:

```rust
        Command::Sidecar(args) => {
            let termination_log = args.termination_log.clone();
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => return balerix_agent::cli::terminate(&termination_log, &e.into()),
            };
            match rt.block_on(balerix_agent::sidecar::main(args)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => balerix_agent::cli::terminate(&termination_log, &e),
            }
        }
```

Run `mise run agent`: `cli_it` still passes (the bundle error text is `bundle::load`'s).

- [ ] **Step 3: The shared fake Daemon**

Move the fake Daemon out of `agent/tests/link_it.rs` into `agent/tests/support/fake_daemon.rs` (declare `pub mod fake_daemon;` in `support/mod.rs`): the `Conn`, `AttachConn`, `Fake` types and the two route handlers as written in Task 12, plus an events route and start/stop:

```rust
pub struct FakeDaemon {
    pub addr: std::net::SocketAddr,
    pub links: mpsc::UnboundedReceiver<Conn>,
    pub attaches: mpsc::UnboundedReceiver<AttachConn>,
    /// Every `hook_event_name` the events route received.
    pub events: mpsc::UnboundedReceiver<String>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

async fn events_route(State(f): State<Fake>, body: axum::body::Bytes) -> axum::Json<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    if let Some(name) = v["hook_event_name"].as_str() {
        let _ = f.events.send(name.to_string());
    }
    axum::Json(serde_json::json!({}))
}

impl FakeDaemon {
    /// On `addr`, or any free port. `stop` ends it; a second `start` on
    /// the same `addr` is "the Daemon came back".
    pub async fn start(addr: Option<std::net::SocketAddr>) -> FakeDaemon {
        let listener = tokio::net::TcpListener::bind(addr.unwrap_or_else(|| "127.0.0.1:0".parse().unwrap())).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (links, links_rx) = mpsc::unbounded_channel();
        let (attaches, attaches_rx) = mpsc::unbounded_channel();
        let (events, events_rx) = mpsc::unbounded_channel();
        let app = Router::new()
            .route("/v1/agents/{f}/{c}/{a}/link", get(link_route))
            .route("/v1/agents/{f}/{c}/{a}/link/attach/{s}", get(attach_route))
            .route("/v1/agents/{f}/{c}/{a}/events", post(events_route))
            .with_state(Fake { links, attaches, events });
        let (stop, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(
            axum::serve(listener, app)
                .with_graceful_shutdown(async { let _ = rx.await; })
                .into_future(),
        );
        FakeDaemon { addr, links: links_rx, attaches: attaches_rx, events: events_rx, stop: Some(stop) }
    }

    pub fn stop(mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}
```

(`Fake` gains `events: mpsc::UnboundedSender<String>`; the file imports `std::future::IntoFuture` for `axum::serve(…).into_future()`.) Open link sockets survive a graceful shutdown; `stop` also has to end them: keep each link handler's loop alive on a `tokio_util`-free signal, the simplest being a `watch::Sender<bool>` in `Fake` that `stop` flips and every handler selects on. Rewrite `link_it.rs` to use `support::fake_daemon::FakeDaemon` and drop its own copy; its assertions do not change.

- [ ] **Step 4: Write the failing two-process tests**

Create `agent/tests/sidecar_it.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §15 "Sidecar and link": a Daemon and a sidecar as two processes,
//! no cluster, the real tools (git, mise, nono, tmux) and `dev
//! fake-claude` in place of `claude`. Skips without the tools or
//! `BALERIX_BIN`; fails instead under `BALERIX_REQUIRE_TOOLS=1`.
mod support;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use balerix_api::{AgentPhase, LinkOp, LinkRequest, LinkResult, SidecarFrame};
use futures_util::StreamExt;
use serde_json::{Value, json};
use support::fake_daemon::{Conn, FakeDaemon};

const BIN: &str = env!("CARGO_BIN_EXE_balerix-agent");
const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn chmod_tree(dir: &Path, dirs: u32, files: u32) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            chmod_tree(&p, dirs, files);
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(dirs)).unwrap();
        } else {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(files)).unwrap();
        }
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(dirs)).unwrap();
}

/// The pod's three mounts under `root`, the crew slice filled the way the
/// Jobs fill it (§8.3): the cache's objects, read-only, and the daemon
/// pool with the embedded tool table (claude, gh) so the agent's own
/// `mise install` finds them and installs nothing.
struct Pod {
    root: PathBuf,
    origin: String,
    bundle: PathBuf,
    ca: PathBuf,
    run: Option<Kill>,
    sidecar: Option<Kill>,
}

impl Pod {
    fn prepare(tools: &support::Tools, label: &str, ca: PathBuf) -> Pod {
        let root = support::temp_root(label);
        for d in ["agent", "shared", "run", "secret"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let origin = root.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "-b", "main"]);
        std::fs::write(origin.join("f"), "one\n").unwrap();
        git(&origin, &["add", "f"]);
        git(&origin, &["commit", "-q", "-m", "one"]);
        let bare = root.join("cache-full");
        assert!(
            Command::new("git").args(["clone", "-q", "--bare"]).arg(&origin).arg(&bare).status().unwrap().success()
        );
        let objects = root.join("shared/repo/.git/objects");
        std::fs::create_dir_all(objects.parent().unwrap()).unwrap();
        assert!(Command::new("cp").arg("-r").arg(bare.join("objects")).arg(&objects).status().unwrap().success());
        chmod_tree(&objects, 0o555, 0o444);
        // the daemon pool
        let pool = root.join("shared/daemon/mise");
        let table = root.join("pool-config");
        std::fs::create_dir_all(&table).unwrap();
        let mut toml = String::from("[tools]\n");
        for (tool, version) in balerix_runtime::embedded_system_tools() {
            toml.push_str(&format!("{tool} = \"{version}\"\n"));
        }
        std::fs::write(table.join("mise.toml"), toml).unwrap();
        let out = Command::new(&tools.mise)
            .current_dir(&table)
            .args(["install"])
            .env("MISE_DATA_DIR", &pool)
            .env("MISE_TRUSTED_CONFIG_PATHS", &table)
            .output()
            .unwrap();
        assert!(out.status.success(), "mise install for the daemon pool: {}", String::from_utf8_lossy(&out.stderr));
        Pod {
            origin: format!("file://{}", origin.display()),
            bundle: root.join("secret/agent.json"),
            ca,
            root,
            run: None,
            sidecar: None,
        }
    }

    fn write_bundle(&self, tools: &support::Tools, daemon_url: &str) {
        std::fs::write(
            &self.bundle,
            json!({
                "agent": "f/c/a",
                "repo": self.origin,
                "git_ref": "main",
                "git": { "push": false, "auth": "none" },
                "settings": {
                    "claude": { "binary": tools.balerix.display().to_string(), "args": ["dev", "fake-claude", "--verbose"] },
                    "sandbox": { "network": { "block": false } }
                },
                "daemon_url": daemon_url,
                "token": TOKEN,
                "credentials": {}
            })
            .to_string(),
        )
        .unwrap();
    }

    fn start(&mut self, tools: &support::Tools, plain_http: bool) {
        let run = Command::new(BIN)
            .args(["run", "--start-timeout-secs", "600", "--run-dir"])
            .arg(self.root.join("run"))
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(self.root.join("run.log")).unwrap()))
            .spawn()
            .unwrap();
        self.run = Some(Kill(run));
        let mut cmd = Command::new(BIN);
        cmd.args(["sidecar", "--hook-port", "0"])
            .arg("--bundle").arg(&self.bundle)
            .arg("--ca").arg(&self.ca)
            .arg("--agent-dir").arg(self.root.join("agent"))
            .arg("--shared-dir").arg(self.root.join("shared"))
            .arg("--run-dir").arg(self.root.join("run"))
            .arg("--balerix").arg(&tools.balerix)
            .arg("--termination-log").arg(self.root.join("termination-log"))
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(self.root.join("sidecar.log")).unwrap()));
        if plain_http {
            cmd.arg("--allow-plain-http");
        }
        self.sidecar = Some(Kill(cmd.spawn().unwrap()));
    }

    fn socket(&self) -> PathBuf {
        self.root.join("run/tmux.sock")
    }
    fn home(&self) -> PathBuf {
        self.root.join("agent/home")
    }
    fn tmux(&self, tools: &support::Tools, args: &[&str]) -> String {
        let out = Command::new(&tools.tmux).arg("-S").arg(self.socket()).arg("-u").args(args).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
    /// The port the profile granted: where Claude posts hooks.
    fn hook_port(&self) -> u16 {
        let profile: Value = serde_json::from_str(
            &std::fs::read_to_string(self.root.join("agent/nono-profile.json")).unwrap(),
        )
        .unwrap();
        profile["network"]["open_port"][0].as_u64().unwrap() as u16
    }
    fn still_running(&mut self) -> bool {
        self.sidecar.as_mut().unwrap().0.try_wait().unwrap().is_none()
    }
}

impl Drop for Pod {
    fn drop(&mut self) {
        self.sidecar.take();
        self.run.take();
        let objects = self.root.join("shared/repo/.git/objects");
        if objects.exists() {
            chmod_tree(&objects, 0o755, 0o644);
        }
        if std::thread::panicking() {
            eprintln!("--- sidecar.log\n{}", std::fs::read_to_string(self.root.join("sidecar.log")).unwrap_or_default());
            eprintln!("--- termination-log\n{}", std::fs::read_to_string(self.root.join("termination-log")).unwrap_or_default());
        }
    }
}

fn ca_file(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    std::fs::write(dir.join("ca.crt"), ca_cert.pem()).unwrap();
    let issuer = rcgen::Issuer::new(ca, ca_key);
    let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = leaf.signed_by(&key, &issuer).unwrap();
    std::fs::write(dir.join("tls.crt"), cert.pem()).unwrap();
    std::fs::write(dir.join("tls.key"), key.serialize_pem()).unwrap();
    (dir.join("ca.crt"), dir.join("tls.crt"), dir.join("tls.key"))
}

async fn next_status(conn: &mut Conn, pred: impl Fn(&balerix_api::LinkStatus) -> bool, what: &str) -> balerix_api::LinkStatus {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, conn.from_sidecar.recv()).await {
            Ok(Some(SidecarFrame::Status(s))) if pred(&s) => return s,
            Ok(Some(_)) => {}
            Ok(None) => panic!("the link closed while waiting for {what}"),
            Err(_) => panic!("timed out waiting for {what}"),
        }
    }
}

async fn request(conn: &mut Conn, id: u64, op: LinkOp) -> LinkResult {
    conn.to_sidecar.send(LinkRequest { id, op }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, conn.from_sidecar.recv()).await {
            Ok(Some(SidecarFrame::Reply(r))) if r.id == id => return r.result,
            Ok(Some(_)) => {}
            _ => panic!("no reply to request {id}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sidecar_against_a_fake_daemon() {
    let Some(tools) = support::tools() else { return };
    let scratch = support::temp_root("sidecar-fake-ca");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &scratch)) {
        return;
    }
    let (ca, _, _) = ca_file(&scratch);
    let mut daemon = FakeDaemon::start(None).await;
    let mut pod = Pod::prepare(&tools, "sidecar-fake", ca);
    pod.write_bundle(&tools, &format!("http://{}", daemon.addr));
    pod.start(&tools, true);

    // the link comes up with the token; SessionStart travels hook-relay →
    // sidecar → Daemon; the agent is Ready and the marker exists
    let mut conn = tokio::time::timeout(Duration::from_secs(180), daemon.links.recv()).await.unwrap().unwrap();
    assert_eq!(conn.headers["authorization"], format!("Bearer {TOKEN}"));
    let ready = next_status(&mut conn, |s| s.status.phase == AgentPhase::Ready, "Ready").await;
    assert!(ready.pid.is_some(), "the pane's pid rides the status frame");
    assert_eq!(tokio::time::timeout(Duration::from_secs(5), daemon.events.recv()).await.unwrap().unwrap(), "SessionStart");
    assert!(pod.root.join("run/ready").is_file());
    assert!(pod.home().join("fake-claude.argv").is_file());

    // send_text over the link reaches fake-claude's stdin
    assert_eq!(request(&mut conn, 1, LinkOp::SendText { text: "hello".into(), submit: true }).await, LinkResult::Ok);
    support::wait_for("hello on stdin", Duration::from_secs(10), || {
        std::fs::read_to_string(pod.home().join("fake-claude.stdin")).is_ok_and(|s| s.contains("hello"))
    });

    // a workspace read runs in the sidecar, in the clone
    assert_eq!(
        request(&mut conn, 2, LinkOp::WorkspaceFile { path: "f".into() }).await,
        LinkResult::File { bytes: b"one\n".to_vec() }
    );
    assert!(matches!(
        request(&mut conn, 3, LinkOp::WorkspaceFile { path: "nope".into() }).await,
        LinkResult::Failed { .. }
    ));

    // attach: the second socket carries the pane
    conn.to_sidecar.send(LinkRequest { id: 4, op: LinkOp::Attach { session: "s1".into() } }).unwrap();
    let mut att = tokio::time::timeout(Duration::from_secs(10), daemon.attaches.recv()).await.unwrap().unwrap();
    assert_eq!(att.headers["authorization"], format!("Bearer {TOKEN}"));
    loop {
        match conn.from_sidecar.recv().await.unwrap() {
            SidecarFrame::Reply(r) if r.id == 4 => { assert_eq!(r.result, LinkResult::Ok); break; }
            _ => {}
        }
    }
    let first = tokio::time::timeout(Duration::from_secs(10), att.from_sidecar.recv()).await.unwrap().unwrap();
    assert!(matches!(first, axum::extract::ws::Message::Binary(b) if !b.is_empty()), "pane bytes");
    drop(att);

    // stop holds; the window is gone; the marker is gone; restart brings
    // a second SessionStart
    assert_eq!(request(&mut conn, 5, LinkOp::Stop).await, LinkResult::Ok);
    next_status(&mut conn, |s| s.status.phase == AgentPhase::Stopped, "Stopped").await;
    support::wait_for("only the anchor window", Duration::from_secs(10), || {
        pod.tmux(&tools, &["list-windows", "-t", "=f/c", "-F", "#{window_name}"]).trim() == "balerix"
    });
    assert!(!pod.root.join("run/ready").exists());
    assert_eq!(request(&mut conn, 6, LinkOp::Restart).await, LinkResult::Ok);
    next_status(&mut conn, |s| s.status.phase == AgentPhase::Ready, "Ready again").await;
    assert_eq!(tokio::time::timeout(Duration::from_secs(5), daemon.events.recv()).await.unwrap().unwrap(), "SessionStart");

    // a dead Claude restarts with the planner's back-off: the supervisor
    // is told to stop (its tree ends, the pane dies), the sidecar notes
    // the exit and restarts; `restarts` counts one
    let pid = pod.tmux(&tools, &["list-windows", "-t", "=f/c", "-F", "#{window_name}\t#{pane_pid}"])
        .lines()
        .find_map(|l| l.strip_prefix("a\t").map(|p| p.trim().to_string()))
        .unwrap();
    assert!(Command::new("kill").args(["-TERM", &pid]).status().unwrap().success());
    let after = next_status(&mut conn, |s| s.status.restarts == 1 && s.status.phase == AgentPhase::Ready, "restarted once").await;
    assert_ne!(after.pid, ready.pid);

    // the Daemon goes away: hooks fail open inside the budget and are
    // counted; the link comes back when the Daemon does
    let addr = daemon.addr;
    daemon.stop();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let port = pod.hook_port();
    let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(10)).build().unwrap();
    let start = Instant::now();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/agents/f/c/a/events"))
        .bearer_auth(TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"hook_event_name":"Stop"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.text().await.unwrap(), "{}");
    assert!(start.elapsed() < Duration::from_millis(3500), "{:?}", start.elapsed());
    let mut daemon = FakeDaemon::start(Some(addr)).await;
    let mut conn = tokio::time::timeout(Duration::from_secs(60), daemon.links.recv()).await.unwrap().unwrap();
    let s = next_status(&mut conn, |_| true, "a status after reconnect").await;
    assert!(s.hook_failures >= 1, "{s:?}");
    assert!(pod.still_running());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sidecar_against_a_real_daemon_over_tls() {
    let Some(tools) = support::tools() else { return };
    let scratch = support::temp_root("sidecar-real-ca");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &scratch)) {
        return;
    }
    let (ca, cert, key) = ca_file(&scratch);
    let token_file = scratch.join("admin-token");
    std::fs::write(&token_file, "fedcba9876543210fedcba9876543210\n").unwrap();
    let daemon_home = scratch.join("daemon-home");
    std::fs::create_dir_all(&daemon_home).unwrap();
    let serve = |bind: &str| {
        Command::new(&tools.balerix)
            .args(["serve", "--mode", "kubernetes", "--bind", bind, "--tmux-socket", "unused"])
            .arg("--tls-cert").arg(&cert)
            .arg("--tls-key").arg(&key)
            .arg("--admin-token-file").arg(&token_file)
            .env("HOME", &daemon_home)
            .env_remove("XDG_CONFIG_HOME").env_remove("XDG_STATE_HOME").env_remove("XDG_DATA_HOME")
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::options().create(true).append(true).open(scratch.join("daemon.log")).unwrap()))
            .spawn()
            .unwrap()
    };
    let endpoint = daemon_home.join(".local/state/balerix/server/endpoint");
    let mut daemon = Kill(serve("127.0.0.1:0"));
    support::wait_for("the endpoint", Duration::from_secs(20), || endpoint.is_file());
    let url = std::fs::read_to_string(&endpoint).unwrap().trim().to_string();
    assert!(url.starts_with("https://"));

    let tls = balerix_agent::tls::client_config(&ca).unwrap();
    let http = balerix_agent::tls::http_client(&tls, Duration::from_secs(10)).unwrap();
    let mut pod = Pod::prepare(&tools, "sidecar-real", ca.clone());
    pod.write_bundle(&tools, &url);
    let spec = json!({
        "spec": { "name": "f", "crews": { "c": {
            "repo": pod.origin, "ref": "main", "git": { "push": false, "auth": "none" },
            "agents": { "a": {
                "claude": { "binary": tools.balerix.display().to_string(), "args": ["dev", "fake-claude", "--verbose"] },
                "sandbox": { "network": { "block": false } }
            } }
        } } },
        "agent_tokens": { "f/c/a": TOKEN }
    });
    let put = |http: &reqwest::Client, url: &str| {
        let (http, url) = (http.clone(), url.to_string());
        let spec = spec.clone();
        async move {
            http.put(format!("{url}/v1/fleets/f"))
                .bearer_auth("fedcba9876543210fedcba9876543210")
                .json(&spec)
                .send()
                .await
                .unwrap()
        }
    };
    let resp = put(&http, &url).await;
    assert_eq!(resp.status().as_u16(), 200, "{}", resp.text().await.unwrap());

    pod.start(&tools, false);
    let phase = |http: &reqwest::Client, url: &str| {
        let (http, url) = (http.clone(), url.to_string());
        async move {
            let v: Value = http
                .get(format!("{url}/v1/fleets/f"))
                .bearer_auth("fedcba9876543210fedcba9876543210")
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            v["status"]["agents"]["f/c/a"].clone()
        }
    };
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let a = phase(&http, &url).await;
        if a["phase"] == "ready" {
            break;
        }
        assert!(Instant::now() < deadline, "never ready: {a}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // the Daemon restarts on the same port; the sidecar reconnects; a
    // forced down from the operator's side travels the link as a stop
    let port = url.rsplit(':').next().unwrap().to_string();
    assert!(Command::new("kill").args(["-TERM", &daemon.0.id().to_string()]).status().unwrap().success());
    support::wait_for("the daemon to exit", Duration::from_secs(20), || daemon.0.try_wait().unwrap().is_some());
    daemon = Kill(serve(&format!("127.0.0.1:{port}")));
    support::wait_for("the endpoint again", Duration::from_secs(20), || {
        std::fs::read_to_string(&endpoint).is_ok_and(|s| s.contains(&port))
    });
    let resp = put(&http, &url).await;
    assert_eq!(resp.status().as_u16(), 200);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let a = phase(&http, &url).await;
        if a["phase"] == "ready" && a["message"] != "link down" {
            break;
        }
        assert!(Instant::now() < deadline, "not back: {a}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let resp = http
        .delete(format!("{url}/v1/fleets/f?keep_repos=false&keep_sessions=false&purge=false&force=true"))
        .bearer_auth("fedcba9876543210fedcba9876543210")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    support::wait_for("the agent window to go", Duration::from_secs(30), || {
        pod.tmux(&tools, &["list-windows", "-t", "=f/c", "-F", "#{window_name}"]).trim() == "balerix"
    });
    assert!(pod.still_running());
    drop(daemon);
}
```

The first status frame the fake sees may be `Pending`; `next_status` skips until the predicate holds. The kill of the supervisor by `-TERM`: `balerix agent-supervise` turns SIGTERM into a stop (`commands/supervise.rs`), ends its tree and exits 143; tmux marks the pane dead; the planner's `NoteExit`, two seconds of back-off, then `Materialize`+`Start`.

- [ ] **Step 5: Run the tier**

Run: `BALERIX_REQUIRE_TOOLS=1 mise run agent`
Expected: both two-process tests pass (each under three minutes: the first `mise install` of claude and gh into the daemon pool is the slow part, cached by mise after the first run); the rest green. If `the_sidecar_against_a_fake_daemon` fails at `next_status(… Ready)`, read `<root>/sidecar.log` and `<root>/agent/logs/*.log`: the usual causes are the profile refusing the hook port (the sidecar bound after rendering) and the server wait (the `run` process did not see the marker).

- [ ] **Step 6: Gate everything and commit**

Run: `mise run check`, `mise run test-it`, `mise run e2e`, `mise run agent`, `mise run plugins`.
Expected: all pass; the tmux mode's behaviour is unchanged (`e2e` is the proof Spec O §18 asks for).

```bash
git add agent
git commit -m "feat(agent): the sidecar: materialise, self-test, start, forward hooks, link, and run the planner for one agent (Spec O §6.2, §6.3)"
```

---

### Task 14: Documents, the by-hand checks, the pull request

**Files:**
- Modify: `docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md` (§7.4 new, §12 three lines, §17.2 status)
- Modify: `ARCHITECTURE.md`, `AGENTS.md`, `docs/THREAT-MODEL.md`
- No code.

- [ ] **Step 1: The spec's §7.4**

Insert after §7.3 (before `## 8. Storage`):

```markdown
### 7.4 Decided in sub-project 2 (2026-10)

- **The Daemon mirrors; it does not plan.** For a fleet with
  `owner: kubernetes` the actor runs no `reconcile_pass`. The sidecar's
  `status` frames are its observed state, written into `status.agents`
  as sent; `SetStopped` becomes a `stop` or `restart` frame; a `status`
  frame that disagrees with the stopped set (a `Ready` agent the set
  holds, a `Stopped` one it does not) gets the frame it missed, which is
  how a sidecar that was away during a `stop` learns of it; `Down`
  sends `stop` to every linked agent, clears the agents and is `Down`
  (the pods are the operator's to delete). Two planners over one agent
  would double every restart.
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
  `DELETE …?force=true`. A tmux-mode daemon answers a body with
  `agent_tokens` 400.
- **Kubernetes mode is flags on `serve`:** `--mode kubernetes --tls-cert
  --tls-key --admin-token-file`, with any `--bind` address and no `-d`.
  The Daemon reads no `plugins.yaml` and launches no plugin; its system
  pool is `Ready` without installing (the shared volume's daemon pool is
  a Job's, §8.3). `GET /readyz` is 200 when Spec F's channel says `Ready`,
  503 with the reason otherwise.
- **The agent container learns its session from the start marker.**
  `balerix-agent run` waits for `<run>/started`, whose content is
  `<fleet>/<crew>`, starts the tmux server with the crew's anchor window
  on `<run>/tmux.sock` and polls `has-session` until the server is gone;
  SIGTERM becomes `kill-server`. It needs no Secret and no arguments.
- **Mount paths are flags with §6.1's defaults** (`--agent-dir
  /balerix/agent`, `--shared-dir /balerix/shared`, `--run-dir
  /balerix/run`, `--bundle /balerix/secret/agent.json`, `--ca
  /balerix/tls/ca.crt`, `--termination-log /dev/termination-log`,
  `--hook-port 7643`). Under the shared mount: `repo/.git/objects`,
  `crew/mise`, `fleet/mise`, `daemon/mise`. Sub-project 3 mounts the
  shared claim's sub-paths there and the Secret at the bundle path.
- **The link's wire shapes** are `balerix-api`'s `link` module: a
  Daemon → sidecar text frame is a `LinkRequest` (`id`, `op` tagged
  `kind`), a sidecar → Daemon frame is a `SidecarFrame` (`reply` with the
  id and a `result` tagged `kind`, or `status` with the agent's
  `AgentStatus`, the pane's pid and the hook-failure count). File bytes
  travel as a JSON array; the 1 MiB file cap keeps that small. The
  connect carries `balerix-link-protocol: 1`.
```

In §12's code layout, add `  src/{cli,bundle,tls,hooks,link,attach,sidecar,run}.rs` under `agent/` and `  kube/{link,pty,idle,tls}.rs` under `crates/balerix-server/src/`; in §17 item 2 append `Done 2026-10 (PR #<n>).` once the PR number exists. Add a line to the Status header: `sub-project 2 decisions in §7.4`.

- [ ] **Step 2: ARCHITECTURE.md, AGENTS.md, THREAT-MODEL.md**

`ARCHITECTURE.md`, in "The pieces" after `balerix-runtime`:

```markdown
- `balerix-agent` (`agent/`, a standalone project like the plugins) — the
  agent pod (Spec O §6, §7). `sidecar` is a one-agent daemon: the core
  planner over `Runtime` and `TmuxRunner` on a pod layout
  (`StateLayout::pod`) and a socket path; it forwards Claude's hooks to the
  Daemon and holds one outbound WebSocket link over which `send_text`,
  `send_keys`, `stop`, `restart`, `attach` and the workspace reads arrive.
  `run` is the agent container's entrypoint: the tmux server, once the
  sidecar's start marker says the agent is materialised.
```

and under `balerix-server`: `… \`kube/\` (Spec O §7: the sidecar link hub implementing \`AgentRunner\` and \`WorkspaceReader\` over the link, the idle ports, TLS serving).` In "Non-obvious decisions", after the Kubernetes-seam bullet:

```markdown
- **A Kubernetes-mode Daemon mirrors; the sidecar plans.** The planner
  runs in the pod, next to tmux; the Daemon's actor takes the sidecar's
  `status` frames as observed state and sends the stopped set as frames.
  Two planners over one agent would double every restart (Spec O §7.4).
- **Pod-mode process waits use no `/proc`.** The pane's pid is the agent
  container's. A stop respawns the pane into a waiter that outlives the
  supervisor and polls tmux's own `pane_dead` (Spec O §6.3, §7.4).
```

`AGENTS.md`: in Tasks, after `plugins`: ``- `agent` — lint and test the standalone `agent/` project (`balerix-agent`, Spec O §12); builds `balerix` first, since its two-process tests run `balerix serve --mode kubernetes` and `launch.sh`. Its own CI job; not part of `check`.`` In Conventions, after the `launch.sh` bullet: ``- `agent/` is standalone like a plugin (own `Cargo.lock`, `deny.toml`, lints), depending on `balerix-api`, `balerix-core` and `balerix-runtime` by path; its TLS and WebSocket stack never reaches the core resolution.`` In Gotchas, after the nono state-root bullet:

```markdown
- `TmuxRunner::at_socket` is the pod runner: `-S <path> -u` on every call,
  and stops wait on `pane_dead` through a waiter the pane is respawned
  into, never on `/proc` (the pid is the agent container's). The
  one-machine runner (`TmuxRunner::new`) is unchanged; `tmux_pod_it`
  covers the other.
- A Kubernetes-mode daemon (`serve --mode kubernetes`) runs no planner
  for its fleets and reads no `plugins.yaml`; `Harness::kube()` is the
  test harness for it, `kube_*_it.rs` the tests.
```

`docs/THREAT-MODEL.md`: a trust boundary after "Plugin ↔ daemon":

```markdown
- **Sidecar ↔ Daemon (the link, Spec O §7)** — one outbound WebSocket per agent pod over TLS, authenticated with the agent's token; carries `send_text`/`send_keys` (keystrokes into the agent), `stop`/`restart`, workspace reads (worktree content out) and, on a second socket, terminal bytes. The Daemon trusts the sidecar's `status` frames as that one agent's state and nothing more. Hook payloads cross it too, forwarded verbatim. **Untrusted input** in both directions: the sidecar runs next to LLM-driven code, the Daemon hosts plugins.
```

Mitigation rows:

```markdown
| A pod reaching another agent's link or status | the link and the attach socket need the agent's own token (constant-time, unknown agent and bad token answer alike, the same index as the hook route); a `status` frame updates only the agent it came from; a `link_attach` session nobody asked for is closed 1008; the protocol header is checked before the upgrade | `crates/balerix-server/src/kube/link.rs`, `hooks.rs` |
| The operator's tokens and the owner rule | `agent_tokens` must name every agent of the spec and no other, 32 characters or more, and is accepted only by a daemon in kubernetes mode (400 otherwise); such a fleet is `owner: kubernetes`, so the CLI's `PUT`/`POST`/`DELETE` answer 409 without `force`; the tokens become the agents' hook secrets and never appear in a log | `daemon.rs::{apply_kube, check_owner}`, `api.rs::update_fleet` |
| The link in the clear | the Daemon serves TLS from the operator's certificate (`axum-server`, rustls on ring); the sidecar trusts the mounted authority alone (no system roots) and refuses an `http://` Daemon except under the hidden `--allow-plain-http` (tests) | `crates/balerix-server/src/kube/tls.rs`, `agent/src/tls.rs`, `agent/src/sidecar.rs` |
| The sidecar's hook ingress | on `127.0.0.1` only, the agent's token in constant time, 1 MiB body, the Daemon's own body validation; a Daemon that cannot be reached inside 3 s is answered as an empty chain (`200 {}`) and counted, so an outage degrades the fleet and never blocks the agent | `agent/src/hooks.rs` |
| A pod stop leaving the tree alive | the waiter respawned into the pane lives while the supervisor does; `pane_dead` is tmux's own view; the 5 s bound fails the stop with `agent processes still running after stop` and keeps the window, as on one machine | `crates/balerix-runtime/src/tmux.rs::{stop_agent, ensure_agent, stop_crew}` (pod arms) |
```

- [ ] **Step 3: Run the gate, commit the documents**

Run: `mise run check` (zizmor/actionlint cover the workflow; the docs change nothing else).

```bash
git add docs ARCHITECTURE.md AGENTS.md
git commit -m "docs: Spec O §7.4 (sub-project 2's decisions), the agent project and kube module in the maps, the link in the threat model"
```

- [ ] **Step 4: The by-hand checks for the claude bump**

AGENTS.md requires both for a `claude` bump; run them on this branch (memory: drivable from a tmux session you start; `verify-claude` pauses for one typed prompt, section H needs the second agent's Enters counted):

```bash
mise run verify-claude
mise run verify-questions
```

Expected: `verify-claude` reports every section passing, section G ends the session, section H prints the Enter count before `UserPromptSubmit`; `verify-questions` reports every dialog shape answered as planned. Paste both reports into the PR body under **By-hand checks (claude 2.1.288)**. A renamed `.claude.json` key or a changed dialog layout in 2.1.288 is a finding to fix in `home.rs` or `plugins/common/src/question.rs` on this branch before the PR, with its own commit.

- [ ] **Step 5: The pull request**

```bash
mise run check && mise run test-it && mise run e2e && mise run agent && mise run plugins
git push -u origin feat/kube-daemon-sidecar
gh pr create --title "feat(server): Kubernetes mode and the balerix-agent sidecar over a TLS link (Spec O §6, §7)" --body-file - <<'EOF'
Spec O sub-project 2 (`docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md` §6, §7, §17.2), plan `docs/superpowers/plans/2026-10-02-balerix-o2-daemon-mode-and-sidecar.md`.

- `balerix serve --mode kubernetes --tls-cert --tls-key --admin-token-file`: TLS, the operator's `PUT /v1/fleets/{name}` with `agent_tokens` (owner `kubernetes`, 409 for the CLI), `/readyz`, no `plugins.yaml`.
- `kube/`: the sidecar link hub (`AgentRunner` + `WorkspaceReader` over one WebSocket per agent, attach on a second socket), the mirror actor, the idle ports.
- `balerix-runtime`: `StateLayout::pod`, materialising over the read-only crew slice, the sandbox self-test, `TmuxRunner::at_socket` with pod-mode waits on `pane_dead`.
- `agent/`: `balerix-agent sidecar` and `run`, with `mise run agent` and a CI job.
- `balerix-api`: the link frames, `AgentBundle`, `FleetRequest.agent_tokens`.
- `mise.toml`: claude 2.1.288.

Decisions the spec left open are recorded as §7.4. Plugins in Kubernetes mode arrive with §9 (sub-project 4); images and charts with §13/§14 (sub-project 5).

## By-hand checks (claude 2.1.288)

<verify-claude report>

<verify-questions report>
EOF
```

Then fill `PR #<n>` into the spec's §17.2 line in a last commit on the branch, and watch CI (`check`, `plugins`, `agent`, `images`).

---

## Self-review (done while writing; the fixes are in the tasks above)

- **Spec coverage.** §6.1 shape: Task 3 (layout) and Task 13 (mounts as flags; the pod's securityContext is the operator's, sub-project 3). §6.2 steps 1–4: Task 13 (1, 2, 3), Task 11 (4, the hook path), Task 4 (`sandbox_self_test`, the slice). §6.3: Task 5 (`-u`, opaque pid, `pane_dead` waits) and Task 13 (the planner in the sidecar). §7.1: Task 11. §7.2: Tasks 2, 6, 8, 12 (every row of both tables; `status` carries phase, pid and restarts through `AgentStatus`). §7.3: Tasks 7 and 9 (`PUT` with tokens, 409s, TLS, `/readyz`); `PUT /v1/plugins` deliberately deferred to §9 with the reason in the header. §12's `agent/` and `kube/` files: Tasks 6–13; §12's `crew_sync.rs`/`harvest.rs` are the Jobs (§8.3, §8.4, sub-project 3) and are not in this plan. §15 "Sidecar and link": Task 13's two tests cover hooks forwarded, `send_text`, attach, workspace reads, link loss and reconnect, the fail-open hook; `stop`/`restart` and the dead-Claude restart as well. §17.2's done-when: Task 13 Step 6.
- **Placeholders.** None: every step has its code or its exact command. Two bodies are explicitly replaced by later tasks (`main.rs`'s `Sidecar` arm in Task 10 → Task 13; `LinkHub::attach` in Task 6 → Task 8), each stated where it is written.
- **Type consistency.** `LinkResult::Failed { failure }` and `LinkFailure { reason, message }` everywhere (Tasks 2, 6, 7, 8, 12, 13); `RunnerError::Link { id, message }` (Tasks 6, 8); `Ports.kube: Option<Arc<LinkHub>>` (Tasks 6, 7, 9); `Msg::Apply.agent_tokens: AgentTokens` (Task 7) with `apply_as`'s last parameter; `PodLayout::{tmux_socket, start_marker, ready_marker}` (Tasks 3, 5, 10, 13); `TmuxRunner::at_socket(PathBuf, PathBuf)` (Tasks 5, 13); `Hooks` fields (Tasks 11, 13); `LinkDeps` fields and `Control` (Tasks 12, 13); `FakeDaemon` fields (Task 13, used by `link_it` after its move).
- **Review Focus.** 1 → Task 5 `observe_parses_without_a_utf8_locale`; 2 → Task 5 `a_pane_process_that_ignores_the_hangup_fails_the_stop_and_keeps_the_window`; 3 → Task 4 `a_pod_materialize_clones_from_the_objects_only_cache_and_writes_nothing_into_it`; 4 → Task 11 `a_daemon_that_is_down_or_slow_fails_open_inside_the_budget` and Task 13's fail-open step; 5 → Task 7 `the_stopped_set_travels_over_the_link_and_is_reconciled_on_reconnect`.
