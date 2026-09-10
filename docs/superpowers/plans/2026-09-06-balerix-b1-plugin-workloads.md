# Balerix Spec B / Phase 1 — Plugin Workloads Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the daemon load, materialize, sandbox and run *plugins* — packages with a `mise.toml` and a `balerix-plugin.yaml` manifest, declared in `$XDG_CONFIG_HOME/balerix/plugins.yaml` — as workloads of the reserved fleet `balerix`, and mark a plugin `Ready` when it calls `POST /v1/plugin-host/hello`. Ends with `balerix dev fake-plugin`, a trivial SDK plugin, reaching `Ready` in the e2e through a real `mise run` under nono.

**Architecture:** `balerix-api` gains the manifest, `plugins.yaml`, hello and status wire types, and the settings block's `flow` becomes `plugins`. `balerix-core` gains `ResolvedPlugin`, manifest validation, the reserved fleet, `Materializer::materialize_plugin`/`purge_plugin`, and `plugin_fleet()`, which renders a plugin list as a synthetic `Fleet` (`balerix`/`plugins`/`<name>`) so the *unchanged* reconciler drives plugins: ids, hashes, backoff, `Stop`/`Start`. `balerix-runtime` materializes a plugin like an agent — `home/`, nono profile, `launch.sh` running `nono run → mise run <start>` from the package directory — under `$XDG_STATE_HOME/balerix/plugins/<name>/`. `balerix-server` gains a `plugins` module: `plugins.yaml` loading, safe tarball unpacking with digest verification, `PluginMaterializer` (maps synthetic agents back to `ResolvedPlugin`s) over a `NullStore`, and `PluginHost`, which owns one ordinary fleet actor for the `balerix` fleet, syncs the installed set to the file, and turns `hello` into that actor's `SessionStart`. The per-agent hook secrets the actor already mints become the per-launch plugin tokens. The binary adds `balerix plugin install|sync|list|remove|package`, `dev fake-plugin`, and wires `PluginHostConfig` into `serve`. A new `balerix-plugin-sdk` crate ships the `Env` and `hello` half of the SDK.

**Tech Stack:** Rust 1.98.1 (edition 2024); tokio 1.53.1, axum 0.8.9, ureq 3.4.1 (no TLS), prometheus 0.14.0; serde/serde_json/serde_norway/toml; sha2 0.11.0; **new:** tar 0.4.46, flate2 1.1.10; thiserror; clap 4; insta, proptest; tempfile, assert_cmd, predicates; real `mise`, `nono 0.75.0`, `tmux 3.7c` for the e2e.

**Spec:** `docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md` (the *plugins spec*), §13 item 1, on top of `docs/superpowers/specs/2026-09-06-balerix-a3-control-plane-design.md` (the *Phase 3 spec*) and `docs/superpowers/specs/2026-09-06-balerix-a2-runtime-design.md` (the *Phase 2 spec*). Read plugins spec §2, §3 and §5 before any task; §4.1 (`hello`) before Task 5–6; §10 before Task 7. Where this plan refines the spec (recorded again in Task 9):

- **Plugins are driven through a synthetic `Fleet`.** `balerix_core::plugin_fleet(&[ResolvedPlugin]) -> Fleet` renders one agent per plugin under fleet `balerix`, crew `plugins`, whose `AgentSettings.env["BALERIX_PLUGIN_HASH"]` is the plugin's hash, and `PluginMaterializer` in the server maps each synthetic agent back to its `ResolvedPlugin` and calls `materialize_plugin`. The spec's "the reconciler is unchanged" holds literally: `plan`/`execute`/`apply` are not touched, and the plugin actor *is* `actor::spawn` with different `Ports`.
- **The plugin token is the actor's hook secret.** The fleet actor already mints a 32-byte hex secret per agent on `Apply` and indexes it in `Shared.hook_secrets`; for the `balerix` fleet that secret is `BALERIX_PLUGIN_TOKEN`, delivered through the `HookTarget` the executor already passes to `materialize`. `hello` authenticates with `Daemon::verify_secret`. No second index, no second minting path.
- **`MISE_GLOBAL_CONFIG_FILE` points at the package's own `mise.toml`**, not a copy under `plugins/<name>/`. The package directory is granted read-only anyway, one file means one `mise trust`, and the in-sandbox `mise run` discovers the same file as the local config (cwd is the package root, `MISE_CEILING_PATHS` stops the walk there).
- **The tmux session is `balerix/plugins`**, the `CrewRef` display form every crew session uses, not `balerix-plugins`.
- **The reserved fleet is rejected at the API and client-side, not in `FleetName`.** `Daemon::apply`/`down` answer 400 and `balerix_config::resolve` errors for a user fleet named `balerix`; `FleetName` itself still parses it, because the plugin actor constructs and re-parses that fleet through the same `Fleet::try_from` path every actor uses.
- **`Materializer` gains two methods, not one:** `materialize_plugin` and `purge_plugin`. `plugin remove --purge` must delete `plugins/<name>/` and the server never knows the layout; the port that knows it is the materializer.
- **`PluginStatus` has no `active_agents` yet**; activation arrives in Phase 2.
- **`plugin install` edits `plugins.yaml` and syncs only if a daemon is running**; otherwise it says `serve` will sync at start. `plugin remove --purge` needs a running daemon.

## Global Constraints

Copied from the specs; every task's requirements include these.

- Rust **1.98.1**, `edition = "2024"`, `rust-version = "1.98"`; every tool in `mise.toml` is an exact version. Run cargo as `mise x -- cargo …` or through `mise run <task>`.
- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` pass at every commit. `unsafe_code = "forbid"`. No `unwrap`/`expect` outside tests (clippy warns; `-D warnings` makes it an error; test modules and `tests/*.rs` carry `#![allow(clippy::unwrap_used, clippy::expect_used)]`). `std::env::set_var` is unsafe in edition 2024 — inject environment through parameters.
- Library crates return `thiserror` errors whose `Display` starts with the id (runtime, server) or config path (config, core: `balerix-plugin.yaml: hooks.intercept: …`, `plugins.yaml: plugins[1].sha256: …`); only the `balerix` binary uses `anyhow`.
- Dependency direction: `api` leaf → `core` → `config` / `runtime` / `server` / `plugin-sdk` (adapters) → binary. `server` never depends on `runtime` or `config`; `plugin-sdk` depends on `api` only; no adapter depends on another adapter.
- New Cargo dependencies go in `[workspace.dependencies]` with an exact version and a reason in the commit message. This plan adds exactly: `tar = "0.4.46"`, `flate2 = "1.1.10"` (package format), and moves `toml`, `sha2`, `serde_norway`, `ureq` into crates that did not have them (no new versions).
- Plain HTTP on `127.0.0.1` only (P3-1). No `rustls`, `rcgen`, `reqwest`, `tonic`.
- Secrets never appear in `Debug` output, logs, argv, the outer environment, or `launch.sh`. The plugin token travels only in `nono-profile.json` (0600) and the `Authorization` header. `balerix_plugin_sdk::Host` hand-implements `Debug` and redacts the token.
- Subprocesses are argv arrays via `std::process::Command`; never a shell string. `launch.sh` passes through `sh_quote`.
- Package unpacking is untrusted input: reject absolute paths, `..`, symlinks, hard links and any entry that is not a regular file or directory; verify the digest before unpacking.
- Names are already validated (`FleetName`, `AgentName`); the runtime and server never build a path from an unvalidated string.
- Integration and e2e tests skip with a printed reason when a tool or Landlock is missing; if `BALERIX_REQUIRE_TOOLS=1` is set (CI), a would-be skip panics instead. Temp roots live under `target/tmp` (`CARGO_TARGET_TMPDIR`), never `/tmp`.
- Commit messages: imperative subject, body explains why, and end with the trailer line `Claude-Session: https://claude.ai/code/session_01AMxZmNaRYWjQoNLWBsL6m6`.
- insta: read every `.snap.new`, compare against the expected values listed in the task, then `mise x -- cargo insta accept`. Never blind-accept.
- The pre-commit hook runs `mise run precommit` (gitleaks + `check`, e2e included, ~30 s). Every commit below goes through it.

## File structure

```
Cargo.toml                                        + balerix-plugin-sdk member/dep; tar, flate2
crates/balerix-api/src/hook.rs                    + HOOK_EVENTS (moved from runtime)
crates/balerix-api/src/plugin.rs                  PluginManifest, HookSubscriptions, Capability, PluginsFile, PluginEntry,
                                                  HelloRequest, HelloResponse, PluginStatus, SyncReport, PLUGIN_PROTOCOL, PLUGIN_KIND
crates/balerix-api/src/settings.rs                flow → plugins: BTreeMap<String, Value>
crates/balerix-api/src/lib.rs                     re-exports
crates/balerix-core/Cargo.toml                    + toml
crates/balerix-core/src/version.rs                is_exact_version (moved from config)
crates/balerix-core/src/plugin.rs                 RESERVED_FLEET, PLUGIN_CREW, plugin_id, ResolvedPlugin, ManifestError,
                                                  validate_manifest, plugin_fleet
crates/balerix-core/src/ports.rs                  Materializer::{materialize_plugin, purge_plugin}
crates/balerix-core/src/fakes.rs                  FakeMaterializer implements both
crates/balerix-core/src/lib.rs                    re-exports
crates/balerix-config/src/validate.rs             plugins block validation; is_exact_version re-export
crates/balerix-config/src/resolve.rs              reserved fleet name rejected
crates/balerix-config/tests/snapshots/*.snap      flow: {} → plugins: {}
examples/payments.yaml                            flow: {} → plugins: {}
crates/balerix-runtime/src/layout.rs              + PluginPaths, StateLayout::{plugin, plugins_data_dir, plugins_state_dir}
crates/balerix-runtime/src/plugin.rs              plugin_env, plugin_grants, render_plugin_launch, write_plugin_home,
                                                  install_plugin_tools, Runtime::render_plugin
crates/balerix-runtime/src/materializer.rs        Materializer::{materialize_plugin, purge_plugin} for Runtime
crates/balerix-runtime/src/home.rs                HOOK_EVENTS re-exported from api
crates/balerix-runtime/src/lib.rs                 re-exports
crates/balerix-runtime/tests/plugin_golden.rs     insta snapshots of the generated plugin files
crates/balerix-runtime/tests/plugin_it.rs         a real package under nono via `mise run`
crates/balerix-server/Cargo.toml                  + serde_norway, sha2, tar, flate2; ureq → dependency
crates/balerix-server/src/plugins/mod.rs          re-exports, PluginError
crates/balerix-server/src/plugins/config.rs       load_plugins_file, Source, resolve_source
crates/balerix-server/src/plugins/manifest.rs     read_manifest (YAML + mise.toml + core validation)
crates/balerix-server/src/plugins/package.rs      sha256_hex, unpack, create, fetch, install
crates/balerix-server/src/plugins/materializer.rs PluginMaterializer, NullStore
crates/balerix-server/src/plugins/host.rs         PluginHostConfig, PluginHost (start, sync, hello, list, purge)
crates/balerix-server/src/daemon.rs               plugins field, sync_plugins, get fallback, reserved rejections, snapshots
crates/balerix-server/src/api.rs                  /v1/plugin-host/hello, /v1/plugins, /v1/plugins/sync, DELETE /v1/plugins/{name}
crates/balerix-server/src/testing.rs              Harness::plugin_config
crates/balerix-server/tests/plugins_it.rs         hello, sync, reserved name, list, purge over the fakes
crates/balerix-plugin-sdk/Cargo.toml
crates/balerix-plugin-sdk/src/lib.rs              Env, Host, SdkError
crates/balerix/Cargo.toml                         + balerix-plugin-sdk
crates/balerix/src/cli.rs                         + plugin {install,sync,list,remove,package}; dev fake-plugin
crates/balerix/src/client.rs                      + plugins, sync_plugins, purge_plugin
crates/balerix/src/commands/plugin.rs             the five subcommands, render_plugins
crates/balerix/src/commands/dev.rs                + fake_plugin_command
crates/balerix/src/commands/serve.rs              PluginHostConfig wiring, fail-fast sync
crates/balerix/src/main.rs                        dispatch
crates/balerix/tests/e2e.rs                       + plugin_hello_journey
ARCHITECTURE.md AGENTS.md README.md docs/THREAT-MODEL.md   plugin updates
docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md   §11.1 verdict, refinements
```

---

### Task 1: `balerix-api` — plugin wire types, `HOOK_EVENTS`, `plugins` settings block

**Files:**
- Create: `crates/balerix-api/src/plugin.rs`
- Modify: `crates/balerix-api/src/hook.rs`, `crates/balerix-api/src/settings.rs`, `crates/balerix-api/src/lib.rs`
- Modify: `crates/balerix-runtime/src/home.rs:19-31` (use the api constant), `crates/balerix-runtime/src/lib.rs:47`
- Modify: `crates/balerix-config/src/validate.rs`, `crates/balerix-config/src/resolve.rs:202`, `crates/balerix-config/tests/snapshots/resolve_golden__payments.snap`, `crates/balerix-config/tests/snapshots/resolve_golden__overrides.snap`, `examples/payments.yaml`

**Interfaces:**
- Produces (used by every later task):
  - `balerix_api::HOOK_EVENTS: &[&str]` — the nine hook events.
  - `balerix_api::plugin::{PluginManifest, HookSubscriptions, Capability, PluginsFile, PluginEntry, HelloRequest, HelloResponse, PluginStatus, SyncReport}`, `PLUGIN_PROTOCOL: u32 = 1`, `PLUGIN_KIND: &str = "Plugin"`.
  - `AgentSettings.plugins: BTreeMap<String, serde_json::Value>` (the field `flow` is gone).

- [ ] **Step 1: Write the failing tests for the wire types**

Create `crates/balerix-api/src/plugin.rs` with only the test module first:

```rust
//! Plugin wire types (plugins spec §2, §3, §4.1): the package manifest, the
//! daemon's `plugins.yaml`, `hello`, status rows and the sync report.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `balerix-api` has no YAML dependency; the manifest is YAML on disk
    /// but JSON-shaped, so the fixture is JSON here.
    fn full_manifest() -> PluginManifest {
        serde_json::from_value(json!({
            "apiVersion": "balerix/v1", "kind": "Plugin", "name": "web", "version": "0.1.0",
            "protocol": 1, "start": "serve",
            "hooks": { "observe": ["SessionStart", "SessionEnd"], "intercept": ["PreToolUse"] },
            "needs": ["fleets", "attach"], "routes": true,
            "sandbox": { "network": { "block": true } }
        }))
        .unwrap()
    }

    #[test]
    fn manifest_parses_with_defaults_for_the_optional_blocks() {
        let m = full_manifest();
        assert_eq!(m.name, "web");
        assert_eq!(m.protocol, 1);
        assert_eq!(m.start, "serve");
        assert!(m.hooks.observe.contains("SessionEnd"));
        assert!(m.hooks.intercept.contains("PreToolUse"));
        assert!(m.needs.contains(&Capability::Fleets));
        assert!(m.needs.contains(&Capability::Attach));
        assert!(m.routes);
        assert_eq!(m.sandbox["network"]["block"], true);

        let minimal: PluginManifest = serde_json::from_value(json!({
            "apiVersion": "balerix/v1", "kind": "Plugin", "name": "x",
            "version": "0.0.1", "protocol": 1, "start": "run"
        }))
        .unwrap();
        assert!(minimal.hooks.observe.is_empty() && minimal.hooks.intercept.is_empty());
        assert!(minimal.needs.is_empty());
        assert!(!minimal.routes);
        assert_eq!(minimal.sandbox, json!({}));
    }

    #[test]
    fn manifest_rejects_unknown_fields_and_capabilities() {
        assert!(
            serde_json::from_value::<PluginManifest>(json!({
                "apiVersion": "balerix/v1", "kind": "Plugin", "name": "x",
                "version": "0.0.1", "protocol": 1, "start": "run", "nope": 1
            }))
            .is_err()
        );
        assert!(serde_json::from_value::<Capability>(json!("root")).is_err());
        assert_eq!(
            serde_json::to_value(Capability::Kv).unwrap(),
            json!("kv"),
            "capabilities are lowercase on the wire"
        );
    }

    #[test]
    fn plugins_file_round_trips_and_keeps_sources_verbatim() {
        let f: PluginsFile = serde_json::from_value(json!({
            "plugins": [
                { "name": "flow", "source": "https://x/flow.tar.gz", "sha256": "ab" },
                { "name": "web", "source": "./web", "config": { "title": "t" } }
            ]
        }))
        .unwrap();
        assert_eq!(f.plugins.len(), 2);
        assert_eq!(f.plugins[0].sha256.as_deref(), Some("ab"));
        assert_eq!(f.plugins[1].sha256, None);
        assert_eq!(f.plugins[1].config["title"], "t");
        assert_eq!(f.plugins[0].config, json!({}));
        let back = serde_json::to_value(&f).unwrap();
        assert!(back["plugins"][1].get("sha256").is_none());
        let empty: PluginsFile = serde_json::from_value(json!({})).unwrap();
        assert!(empty.plugins.is_empty());
        assert!(serde_json::from_value::<PluginsFile>(json!({ "plugin": [] })).is_err());
    }

    #[test]
    fn hello_status_and_report_round_trip() {
        let h = HelloRequest {
            name: "web".into(),
            version: "0.1.0".into(),
            protocol: PLUGIN_PROTOCOL,
            listen: "127.0.0.1:4321".into(),
        };
        let back: HelloRequest = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(back, h);
        let r: HelloResponse = serde_json::from_value(json!({ "config": { "a": 1 } })).unwrap();
        assert_eq!(r.config["a"], 1);
        let s = PluginStatus {
            name: "web".into(),
            version: "0.1.0".into(),
            phase: crate::AgentPhase::Ready,
            listen: Some("127.0.0.1:4321".into()),
            routes: true,
            message: String::new(),
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["phase"], "ready");
        let rep = SyncReport::default();
        assert!(rep.installed.is_empty() && rep.stopped.is_empty() && rep.unchanged.is_empty());
        assert_eq!(PLUGIN_KIND, "Plugin");
    }
}
```

Add `pub mod plugin;` to `crates/balerix-api/src/lib.rs`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-api plugin`
Expected: FAIL to compile — `PluginManifest`, `Capability`, … not found.

- [ ] **Step 3: Write the wire types**

Above the test module in `crates/balerix-api/src/plugin.rs`:

```rust
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AgentPhase;

/// Host protocol major this daemon speaks (plugins spec §4).
pub const PLUGIN_PROTOCOL: u32 = 1;
/// `kind` of every manifest.
pub const PLUGIN_KIND: &str = "Plugin";

/// `balerix-plugin.yaml` at a package root (plugins spec §2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub version: String,
    pub protocol: u32,
    /// The mise task that starts the plugin.
    pub start: String,
    #[serde(default)]
    pub hooks: HookSubscriptions,
    #[serde(default)]
    pub needs: BTreeSet<Capability>,
    #[serde(default)]
    pub routes: bool,
    /// nono-mirroring YAML merged over balerix's base profile; passthrough.
    #[serde(default = "empty_object")]
    pub sandbox: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookSubscriptions {
    #[serde(default)]
    pub observe: BTreeSet<String>,
    #[serde(default)]
    pub intercept: BTreeSet<String>,
}

/// Host capabilities a plugin may declare (plugins spec §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    Fleets,
    Actions,
    Attach,
    Kv,
}

/// `$XDG_CONFIG_HOME/balerix/plugins.yaml` (plugins spec §2.1). Order is
/// the interceptor order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsFile {
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginEntry {
    pub name: String,
    /// `https://` URL, tarball path, or directory path (relative to the file).
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Daemon-level config, passed verbatim in the `hello` reply.
    #[serde(default = "empty_object")]
    pub config: Value,
}

/// Body of `POST /v1/plugin-host/hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloRequest {
    pub name: String,
    pub version: String,
    pub protocol: u32,
    /// `127.0.0.1:<port>` the plugin listens on.
    pub listen: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloResponse {
    pub config: Value,
}

/// One row of `GET /v1/plugins`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginStatus {
    pub name: String,
    pub version: String,
    pub phase: AgentPhase,
    #[serde(default)]
    pub listen: Option<String>,
    pub routes: bool,
    #[serde(default)]
    pub message: String,
}

/// What `POST /v1/plugins/sync` did, by plugin name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncReport {
    pub installed: Vec<String>,
    pub stopped: Vec<String>,
    pub unchanged: Vec<String>,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}
```

In `crates/balerix-api/src/hook.rs`, after the imports:

```rust
/// Every Claude Code hook event balerix wires (Phase 2 spec §4.2). One
/// list so the settings writer, the daemon and plugin manifests agree.
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
```

In `crates/balerix-api/src/lib.rs` replace the `pub use hook::HookEvent;` line and add plugin exports:

```rust
pub use hook::{HOOK_EVENTS, HookEvent};
pub use plugin::{
    Capability, HelloRequest, HelloResponse, HookSubscriptions, PLUGIN_KIND, PLUGIN_PROTOCOL,
    PluginEntry, PluginManifest, PluginStatus, PluginsFile, SyncReport,
};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo test -p balerix-api plugin`
Expected: PASS (4 tests).

- [ ] **Step 5: Rename `flow` to `plugins` in the settings block (test first)**

In `crates/balerix-api/src/settings.rs` tests, replace `assert_eq!(s.flow, json!({}));` with `assert!(s.plugins.is_empty());`, replace `"flow": {}` in `deserializes_a_full_block` with `"plugins": { "web": { "enabled": true } }` and add after the runner assertion:

```rust
        assert_eq!(s.plugins["web"]["enabled"], true);
```

Add a test:

```rust
    #[test]
    fn plugins_is_a_map_of_passthrough_objects() {
        let s: AgentSettings = serde_json::from_value(
            json!({ "plugins": { "flow": { "initial": "working" }, "web": {} } }),
        )
        .unwrap();
        assert_eq!(s.plugins.len(), 2);
        assert_eq!(s.plugins["flow"]["initial"], "working");
        assert!(serde_json::from_value::<AgentSettings>(json!({ "flow": {} })).is_err());
    }
```

Run: `mise x -- cargo test -p balerix-api settings`
Expected: FAIL to compile (`no field plugins`).

- [ ] **Step 6: Make the rename**

In `crates/balerix-api/src/settings.rs` replace the `flow` field and its default:

```rust
    /// Plugin name → that plugin's per-agent config (plugins spec §2.2);
    /// passthrough objects, merged like every other map.
    #[serde(default)]
    pub plugins: BTreeMap<String, Value>,
```

and in `Default`: `plugins: BTreeMap::new(),`.

Run: `mise x -- cargo test -p balerix-api`
Expected: PASS.

- [ ] **Step 7: Move `HOOK_EVENTS` out of the runtime and fix the config crate**

In `crates/balerix-runtime/src/home.rs` delete the `pub const HOOK_EVENTS` block (lines 19–31) and add near the top: `pub use balerix_api::HOOK_EVENTS;`. `crates/balerix-runtime/src/lib.rs` line 47 keeps exporting `HOOK_EVENTS` from `home` unchanged.

In `crates/balerix-config/src/validate.rs`:
- change the message on the `hooks` rejection to `"balerix owns this key; configure hook behaviour via `plugins` instead"`;
- replace the `flow` check with:

```rust
    for (name, cfg) in &settings.plugins {
        if let Err(reason) = balerix_core::name::validate_name(name) {
            return Err(invalid(
                &format!("plugins.{name}"),
                format!("invalid plugin name: {reason}"),
            ));
        }
        if !cfg.is_object() {
            return Err(invalid(
                &format!("plugins.{name}"),
                "expected a mapping".to_string(),
            ));
        }
    }
```

- in the tests: update the expected `hooks` message (two places, `validate.rs` and `resolve.rs:202`); replace the `flow: json!("x")` case in `passthrough_blocks_must_be_maps` with:

```rust
        let s = AgentSettings {
            plugins: BTreeMap::from([("web".to_string(), json!("x"))]),
            ..AgentSettings::default()
        };
        assert_eq!(
            validate_agent("p", &s).unwrap_err().to_string(),
            "p.plugins.web: expected a mapping"
        );
        let s = AgentSettings {
            plugins: BTreeMap::from([("Web".to_string(), json!({}))]),
            ..AgentSettings::default()
        };
        assert_eq!(
            validate_agent("p", &s).unwrap_err().to_string(),
            "p.plugins.Web: invalid plugin name: contains characters other than a-z, 0-9 and '-'"
        );
```

`balerix_core::name::validate_name` is already `pub` (`crates/balerix-core/src/name.rs:70`); `balerix-config` already depends on `balerix-core`.

In `examples/payments.yaml` replace `  flow: {}` with `  plugins: {}`. In both config snapshots replace every `flow: {}` line with `plugins: {}` (same indentation).

- [ ] **Step 8: Run the workspace check**

Run: `mise run check`
Expected: PASS. If `resolve_golden` produces `.snap.new` files, open them, confirm the only difference is `flow: {}` → `plugins: {}`, then `mise x -- cargo insta accept`.

- [ ] **Step 9: Commit**

```bash
git add -A crates/balerix-api crates/balerix-runtime/src/home.rs crates/balerix-config examples/payments.yaml
git commit -m "$(cat <<'EOF'
Add the plugin wire types and rename the settings block's flow to plugins

Plugins spec §2–§4: the manifest, plugins.yaml, hello and status DTOs live
in balerix-api as serde types with no logic. The reserved `flow: {}` block
becomes `plugins: { <name>: {…} }`, one map for every plugin's per-agent
config, and HOOK_EVENTS moves to the api crate so manifests can be
validated against the same list the settings writer uses.

Claude-Session: https://claude.ai/code/session_01AMxZmNaRYWjQoNLWBsL6m6
EOF
)"
```

---

### Task 2: `balerix-core` — `ResolvedPlugin`, manifest validation, the reserved fleet, port methods

**Files:**
- Create: `crates/balerix-core/src/plugin.rs`, `crates/balerix-core/src/version.rs`
- Modify: `crates/balerix-core/Cargo.toml`, `crates/balerix-core/src/lib.rs`, `crates/balerix-core/src/ports.rs:610-628`, `crates/balerix-core/src/fakes.rs`
- Modify: `crates/balerix-config/src/validate.rs` (use the moved `is_exact_version`), `crates/balerix-config/src/lib.rs:16`, `crates/balerix-config/src/resolve.rs`

**Interfaces:**
- Consumes: Task 1's `PluginManifest`, `HOOK_EVENTS`, `PLUGIN_PROTOCOL`, `PLUGIN_KIND`, `API_VERSION`.
- Produces:
  - `balerix_core::RESERVED_FLEET: &str = "balerix"`, `PLUGIN_CREW: &str = "plugins"`, `pub fn is_reserved_fleet(name: &str) -> bool`, `pub fn plugin_id(name: &AgentName) -> AgentId`.
  - `pub struct ResolvedPlugin { pub name: AgentName, pub package: PathBuf, pub manifest: PluginManifest, pub config: Value, pub digest: Option<String> }` with `fn id(&self) -> AgentId` and `fn hash(&self) -> SpecHash`.
  - `pub enum ManifestError { Manifest { path, message }, MiseToml { path, message } }`; `pub fn validate_manifest(m: &PluginManifest, mise_toml: &str) -> Result<(), ManifestError>`.
  - `pub fn plugin_fleet(plugins: &[ResolvedPlugin]) -> Fleet`.
  - `balerix_core::is_exact_version(&str) -> bool` (re-exported by `balerix_config`).
  - `Materializer::materialize_plugin(&self, plugin: &ResolvedPlugin, host: &HookTarget) -> Result<LaunchPlan, MaterializeError>` and `Materializer::purge_plugin(&self, name: &AgentName) -> Result<(), MaterializeError>`.
  - `FakeMaterializer` records `"materialize_plugin balerix/plugins/<name>"` and `"purge_plugin <name>"`, failable through `fail_next` with those method names.

- [ ] **Step 1: Move `is_exact_version` into core (test first)**

Create `crates/balerix-core/src/version.rs` by moving `is_exact_version` and its two tests (`exact_versions_are_accepted`, `fuzzy_versions_are_rejected`) verbatim from `crates/balerix-config/src/validate.rs`. In `validate.rs` delete the function and those two tests, and add `use balerix_core::is_exact_version;`. In `crates/balerix-config/src/lib.rs` change the last line to:

```rust
pub use balerix_core::is_exact_version;
pub use validate::{RESERVED_ENV_PREFIXES, validate_agent};
```

Add `pub mod version;` and `pub use version::is_exact_version;` to `crates/balerix-core/src/lib.rs`.

Run: `mise x -- cargo test -p balerix-core version && mise x -- cargo test -p balerix-config`
Expected: PASS.

- [ ] **Step 2: Write the failing tests for `plugin.rs`**

Create `crates/balerix-core/src/plugin.rs`:

```rust
//! Plugins as the daemon sees them (plugins spec §3, §5.2): the resolved
//! package, manifest validation, and the synthetic fleet the reconciler
//! drives them through.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResolvedAgent;
    use balerix_api::{Capability, HookSubscriptions};
    use serde_json::json;
    use std::collections::BTreeSet;

    const MISE: &str = "[tools]\nttyd = \"1.7.7\"\n\n[tasks.serve]\nrun = \"python3 plugin.py\"\n";

    fn manifest(name: &str) -> PluginManifest {
        PluginManifest {
            api_version: "balerix/v1".into(),
            kind: "Plugin".into(),
            name: name.into(),
            version: "0.1.0".into(),
            protocol: 1,
            start: "serve".into(),
            hooks: HookSubscriptions {
                observe: BTreeSet::from(["SessionStart".to_string()]),
                intercept: BTreeSet::new(),
            },
            needs: BTreeSet::from([Capability::Fleets]),
            routes: true,
            sandbox: json!({}),
        }
    }

    fn plugin(name: &str, config: serde_json::Value) -> ResolvedPlugin {
        ResolvedPlugin {
            name: name.parse().unwrap(),
            package: format!("/pkg/{name}").into(),
            manifest: manifest(name),
            config,
            digest: Some("abc".into()),
        }
    }

    #[test]
    fn the_daemon_fleet_is_reserved_and_plugin_ids_live_under_it() {
        assert!(is_reserved_fleet("balerix"));
        assert!(!is_reserved_fleet("payments"));
        assert_eq!(
            plugin_id(&"web".parse().unwrap()).to_string(),
            "balerix/plugins/web"
        );
        assert_eq!(plugin("web", json!({})).id().to_string(), "balerix/plugins/web");
    }

    #[test]
    fn a_valid_manifest_passes() {
        validate_manifest(&manifest("web"), MISE).unwrap();
    }

    #[test]
    fn manifest_errors_carry_the_config_path() {
        let cases: Vec<(Box<dyn Fn(&mut PluginManifest)>, &str)> = vec![
            (
                Box::new(|m| m.api_version = "balerix/v2".into()),
                "balerix-plugin.yaml: apiVersion: expected \"balerix/v1\", got \"balerix/v2\"",
            ),
            (
                Box::new(|m| m.kind = "Fleet".into()),
                "balerix-plugin.yaml: kind: expected \"Plugin\", got \"Fleet\"",
            ),
            (
                Box::new(|m| m.name = "Web".into()),
                "balerix-plugin.yaml: name: invalid plugin name \"Web\": contains characters other than a-z, 0-9 and '-'",
            ),
            (
                Box::new(|m| m.version = " ".into()),
                "balerix-plugin.yaml: version: must not be empty",
            ),
            (
                Box::new(|m| m.protocol = 2),
                "balerix-plugin.yaml: protocol: this daemon speaks protocol 1, got 2",
            ),
            (
                Box::new(|m| m.start = String::new()),
                "balerix-plugin.yaml: start: must not be empty",
            ),
            (
                Box::new(|m| {
                    m.hooks.intercept.insert("Foo".into());
                }),
                "balerix-plugin.yaml: hooks.intercept: unknown event \"Foo\"",
            ),
            (
                Box::new(|m| {
                    m.hooks.observe.insert("Bar".into());
                }),
                "balerix-plugin.yaml: hooks.observe: unknown event \"Bar\"",
            ),
            (
                Box::new(|m| m.sandbox = json!([1])),
                "balerix-plugin.yaml: sandbox: expected a mapping",
            ),
        ];
        for (mutate, expected) in cases {
            let mut m = manifest("web");
            mutate(&mut m);
            assert_eq!(validate_manifest(&m, MISE).unwrap_err().to_string(), expected);
        }
    }

    #[test]
    fn mise_toml_must_parse_pin_exactly_and_define_the_start_task() {
        let e = validate_manifest(&manifest("web"), "[tools\n").unwrap_err();
        assert!(e.to_string().starts_with("mise.toml: "), "{e}");
        assert_eq!(
            validate_manifest(
                &manifest("web"),
                "[tools]\nttyd = \"latest\"\n[tasks.serve]\nrun = \"x\"\n"
            )
            .unwrap_err()
            .to_string(),
            "mise.toml: tools.ttyd: expected an exact version, got \"latest\""
        );
        assert_eq!(
            validate_manifest(&manifest("web"), "[tools]\n").unwrap_err().to_string(),
            "mise.toml: tasks.serve: the manifest's `start` task is not defined"
        );
        // inline table form of tasks is accepted too
        validate_manifest(&manifest("web"), "tasks = { serve = \"python3 p.py\" }\n").unwrap();
        // a tools entry in table form with a version key
        validate_manifest(
            &manifest("web"),
            "[tools]\nnode = { version = \"22.11.0\" }\n[tasks.serve]\nrun = \"x\"\n",
        )
        .unwrap();
    }

    #[test]
    fn hash_covers_package_manifest_config_and_digest() {
        let a = plugin("web", json!({ "title": "t" }));
        assert_eq!(a.hash(), plugin("web", json!({ "title": "t" })).hash());
        let mut b = a.clone();
        b.config = json!({ "title": "u" });
        assert_ne!(a.hash(), b.hash());
        let mut c = a.clone();
        c.package = "/elsewhere".into();
        assert_ne!(a.hash(), c.hash());
        let mut d = a.clone();
        d.digest = None;
        assert_ne!(a.hash(), d.hash());
        let mut e = a.clone();
        e.manifest.routes = false;
        assert_ne!(a.hash(), e.hash());
        assert_eq!(a.hash().as_str().len(), 64);
    }

    #[test]
    fn plugin_fleet_renders_one_agent_per_plugin_whose_hash_tracks_the_plugin() {
        let plugins = vec![plugin("web", json!({})), plugin("flow", json!({ "x": 1 }))];
        let fleet = plugin_fleet(&plugins);
        assert_eq!(fleet.name.as_str(), RESERVED_FLEET);
        let crew = &fleet.crews[&PLUGIN_CREW.parse().unwrap()];
        assert_eq!(crew.agents.len(), 2);
        let agents = ResolvedAgent::from_fleet(&fleet);
        assert_eq!(agents[0].id.to_string(), "balerix/plugins/flow");
        assert_eq!(agents[1].id.to_string(), "balerix/plugins/web");
        assert_eq!(
            agents[1].settings.env["BALERIX_PLUGIN_HASH"],
            plugins[0].hash().as_str()
        );
        // a plugin change changes the synthetic agent's hash; an unchanged one does not
        let before = agents[1].hash();
        let same = ResolvedAgent::from_fleet(&plugin_fleet(&plugins));
        assert_eq!(same[1].hash(), before);
        let mut changed = plugins.clone();
        changed[0].config = json!({ "title": "x" });
        let after = ResolvedAgent::from_fleet(&plugin_fleet(&changed));
        assert_ne!(after[1].hash(), before);
        // the empty list is a valid fleet with one empty crew
        let empty = plugin_fleet(&[]);
        assert!(empty.crews[&PLUGIN_CREW.parse().unwrap()].agents.is_empty());
    }
}
```

Add `pub mod plugin;` to `crates/balerix-core/src/lib.rs`.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-core plugin`
Expected: FAIL to compile.

- [ ] **Step 4: Implement `plugin.rs`**

Add `toml = { workspace = true }` to `crates/balerix-core/Cargo.toml` `[dependencies]`. Above the tests in `plugin.rs`:

```rust
use std::collections::BTreeMap;
use std::path::PathBuf;

use balerix_api::{
    API_VERSION, AgentSettings, GitAuth, GitSettings, HOOK_EVENTS, PLUGIN_KIND, PLUGIN_PROTOCOL,
    PluginManifest, SpecHash,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::fleet::{Crew, Fleet};
use crate::name::{AgentId, AgentName, validate_name};
use crate::repo::RepoRef;
use crate::version::is_exact_version;

/// The daemon's own fleet: every plugin is an agent of its `plugins` crew.
/// User fleets may not take this name (checked at the API and client-side;
/// `FleetName` itself still parses it because the plugin actor round-trips
/// it through `Fleet::try_from` like every other fleet).
pub const RESERVED_FLEET: &str = "balerix";
pub const PLUGIN_CREW: &str = "plugins";

pub fn is_reserved_fleet(name: &str) -> bool {
    name == RESERVED_FLEET
}

/// `balerix/plugins/<name>`.
pub fn plugin_id(name: &AgentName) -> AgentId {
    AgentId {
        // Both literals satisfy `validate_name`; a failure here would be a
        // programming error, so fall through to the unchecked constructor.
        fleet: RESERVED_FLEET.parse().unwrap_or_else(|_| unreachable!()),
        crew: PLUGIN_CREW.parse().unwrap_or_else(|_| unreachable!()),
        agent: name.clone(),
    }
}

/// One plugin after `plugins.yaml` was synced: where its package is, what
/// its manifest says, and the daemon-level config it gets at `hello`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedPlugin {
    pub name: AgentName,
    /// Package root (installed tarball or a directory used in place).
    pub package: PathBuf,
    pub manifest: PluginManifest,
    pub config: Value,
    /// sha256 hex of the tarball; `None` for a directory source.
    pub digest: Option<String>,
}

#[derive(Serialize)]
struct HashInput<'a> {
    package: String,
    manifest: &'a PluginManifest,
    config: &'a Value,
    digest: &'a Option<String>,
}

impl ResolvedPlugin {
    pub fn id(&self) -> AgentId {
        plugin_id(&self.name)
    }

    /// Exactly what, when changed, must restart the plugin.
    pub fn hash(&self) -> SpecHash {
        let input = HashInput {
            package: self.package.display().to_string(),
            manifest: &self.manifest,
            config: &self.config,
            digest: &self.digest,
        };
        let bytes = serde_json::to_vec(&input).unwrap_or_default();
        SpecHash::new(hex::encode(Sha256::digest(bytes)))
    }
}

/// Why a package is not a valid plugin. Messages start with the file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("balerix-plugin.yaml: {path}: {message}")]
    Manifest { path: String, message: String },
    #[error("mise.toml: {path}: {message}")]
    MiseToml { path: String, message: String },
}

/// Plugins spec §2 rules, applied to a parsed manifest and the text of the
/// package's `mise.toml`.
pub fn validate_manifest(m: &PluginManifest, mise_toml: &str) -> Result<(), ManifestError> {
    let bad = |path: &str, message: String| ManifestError::Manifest {
        path: path.to_string(),
        message,
    };
    if m.api_version != API_VERSION {
        return Err(bad(
            "apiVersion",
            format!("expected {API_VERSION:?}, got {:?}", m.api_version),
        ));
    }
    if m.kind != PLUGIN_KIND {
        return Err(bad("kind", format!("expected {PLUGIN_KIND:?}, got {:?}", m.kind)));
    }
    if let Err(reason) = validate_name(&m.name) {
        return Err(bad(
            "name",
            format!("invalid plugin name {:?}: {reason}", m.name),
        ));
    }
    if m.version.trim().is_empty() {
        return Err(bad("version", "must not be empty".into()));
    }
    if m.protocol != PLUGIN_PROTOCOL {
        return Err(bad(
            "protocol",
            format!("this daemon speaks protocol {PLUGIN_PROTOCOL}, got {}", m.protocol),
        ));
    }
    if m.start.trim().is_empty() {
        return Err(bad("start", "must not be empty".into()));
    }
    for (list, events) in [
        ("hooks.observe", &m.hooks.observe),
        ("hooks.intercept", &m.hooks.intercept),
    ] {
        if let Some(e) = events.iter().find(|e| !HOOK_EVENTS.contains(&e.as_str())) {
            return Err(bad(list, format!("unknown event {e:?}")));
        }
    }
    if !m.sandbox.is_object() {
        return Err(bad("sandbox", "expected a mapping".into()));
    }
    validate_mise_toml(mise_toml, &m.start)
}

fn validate_mise_toml(text: &str, start: &str) -> Result<(), ManifestError> {
    let doc: toml::Table = text.parse().map_err(|e: toml::de::Error| ManifestError::MiseToml {
        path: String::new(),
        message: e.message().to_string(),
    })?;
    if let Some(toml::Value::Table(tools)) = doc.get("tools") {
        for (k, v) in tools {
            let version = match v {
                toml::Value::String(s) => s.clone(),
                toml::Value::Table(t) => match t.get("version") {
                    Some(toml::Value::String(s)) => s.clone(),
                    _ => {
                        return Err(ManifestError::MiseToml {
                            path: format!("tools.{k}"),
                            message: "expected a version string".into(),
                        });
                    }
                },
                _ => {
                    return Err(ManifestError::MiseToml {
                        path: format!("tools.{k}"),
                        message: "expected a version string".into(),
                    });
                }
            };
            if !is_exact_version(&version) {
                return Err(ManifestError::MiseToml {
                    path: format!("tools.{k}"),
                    message: format!("expected an exact version, got {version:?}"),
                });
            }
        }
    }
    let defined = matches!(doc.get("tasks"), Some(toml::Value::Table(t)) if t.contains_key(start));
    if !defined {
        return Err(ManifestError::MiseToml {
            path: format!("tasks.{start}"),
            message: "the manifest's `start` task is not defined".into(),
        });
    }
    Ok(())
}

/// The synthetic fleet the reconciler drives plugins through (plugins spec
/// §5.2): fleet `balerix`, crew `plugins`, one agent per plugin. The agent's
/// settings carry nothing but the plugin's hash, so `ResolvedAgent::hash`
/// changes exactly when the plugin must restart. `PluginMaterializer` in
/// the server maps the ids back to `ResolvedPlugin`s; the repo is a
/// placeholder its `ensure_crew` never touches.
pub fn plugin_fleet(plugins: &[ResolvedPlugin]) -> Fleet {
    let agents: BTreeMap<AgentName, AgentSettings> = plugins
        .iter()
        .map(|p| {
            let mut s = AgentSettings::default();
            s.env
                .insert("BALERIX_PLUGIN_HASH".to_string(), p.hash().as_str().to_string());
            (p.name.clone(), s)
        })
        .collect();
    Fleet {
        name: RESERVED_FLEET.parse().unwrap_or_else(|_| unreachable!()),
        crews: BTreeMap::from([(
            PLUGIN_CREW.parse().unwrap_or_else(|_| unreachable!()),
            Crew {
                repo: RepoRef::Local(PathBuf::from("/dev/null")),
                git_ref: "none".to_string(),
                git: GitSettings {
                    push: false,
                    auth: GitAuth::None,
                    identity: None,
                },
                agents,
            },
        )]),
    }
}
```

`ManifestError::MiseToml` with an empty `path` displays as `mise.toml: : <msg>`; make the `Display` clean by giving the parse error path `"(parse)"` instead of `String::new()` — the test only checks the `mise.toml: ` prefix. Use `path: "(parse)".into()`.

Note: `toml::de::Error::message()` exists in toml 1.x; if clippy or the compiler disagrees, use `e.to_string()` and take its first line via `crate::first_line`.

Add to `crates/balerix-core/src/lib.rs`:

```rust
pub use plugin::{
    ManifestError, PLUGIN_CREW, RESERVED_FLEET, ResolvedPlugin, is_reserved_fleet, plugin_fleet,
    plugin_id, validate_manifest,
};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `mise x -- cargo test -p balerix-core plugin`
Expected: PASS (6 tests).

- [ ] **Step 6: Extend the `Materializer` port and the fake (test first)**

In `crates/balerix-core/src/fakes.rs` tests add:

```rust
    #[test]
    fn plugin_calls_are_recorded_and_failable() {
        let m = FakeMaterializer::default();
        let p = ResolvedPlugin {
            name: "web".parse().unwrap(),
            package: "/pkg/web".into(),
            manifest: serde_json::from_value(serde_json::json!({
                "apiVersion": "balerix/v1", "kind": "Plugin", "name": "web",
                "version": "0.1.0", "protocol": 1, "start": "serve"
            }))
            .unwrap(),
            config: serde_json::json!({}),
            digest: None,
        };
        let host = HookTarget {
            url: "http://127.0.0.1:1".into(),
            secret: "tok".into(),
        };
        let plan = m.materialize_plugin(&p, &host).unwrap();
        assert_eq!(plan.cwd, PathBuf::from("/pkg/web"));
        m.fail_next("purge_plugin", "web", "busy");
        assert_eq!(
            m.purge_plugin(&"web".parse().unwrap())
                .unwrap_err()
                .to_string(),
            "web: rm purge_plugin: busy"
        );
        assert_eq!(
            m.calls(),
            vec!["materialize_plugin balerix/plugins/web", "purge_plugin web"]
        );
    }
```

Run: `mise x -- cargo test -p balerix-core fakes`
Expected: FAIL to compile (`no method materialize_plugin`).

- [ ] **Step 7: Add the methods**

In `crates/balerix-core/src/ports.rs` add to `Materializer` after `remove_crew`:

```rust
    /// Files for one plugin (plugins spec §5.1): `home/`, profile,
    /// `launch.sh`, tools installed. `host` carries the daemon URL and the
    /// plugin's per-launch token.
    fn materialize_plugin(
        &self,
        plugin: &ResolvedPlugin,
        host: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError>;
    /// Deletes `plugins/<name>/` — kv, scratch, home, everything
    /// (`plugin remove --purge`). Not part of any reconcile pass.
    fn purge_plugin(&self, name: &AgentName) -> Result<(), MaterializeError>;
```

with `use crate::plugin::ResolvedPlugin;` added to the imports. In `fakes.rs` implement for `FakeMaterializer`:

```rust
    fn materialize_plugin(
        &self,
        plugin: &ResolvedPlugin,
        _: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        let id = plugin.id().to_string();
        self.check("materialize_plugin", &id, "mise")?;
        Ok(LaunchPlan {
            cwd: plugin.package.clone(),
            env: BTreeMap::new(),
            argv: vec!["fake-plugin".into()],
            script: PathBuf::from("/fake").join(&id).join("launch.sh"),
        })
    }
    fn purge_plugin(&self, name: &AgentName) -> Result<(), MaterializeError> {
        self.check("purge_plugin", name.as_str(), "rm")
    }
```

(imports: `use crate::name::{AgentId, AgentName, FleetName};` and `use crate::plugin::ResolvedPlugin;`).

The `Runtime` in `balerix-runtime` also implements `Materializer`; until Task 3 lands it will not compile. Add a temporary implementation there now so the workspace builds:

```rust
    fn materialize_plugin(
        &self,
        plugin: &ResolvedPlugin,
        _: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        Err(MaterializeError::Invalid {
            id: plugin.id().to_string(),
            message: "plugins are not materializable yet (Spec B phase 1, task 3)".into(),
        })
    }
    fn purge_plugin(&self, name: &AgentName) -> Result<(), MaterializeError> {
        Err(MaterializeError::Invalid {
            id: name.to_string(),
            message: "plugins are not materializable yet (Spec B phase 1, task 3)".into(),
        })
    }
```

(with `use balerix_core::{AgentName, ResolvedPlugin}` added to `materializer.rs` imports). Task 3 replaces both bodies.

- [ ] **Step 8: Reject the reserved fleet name client-side (test first)**

In `crates/balerix-config/src/resolve.rs` tests add:

```rust
    #[test]
    fn the_daemons_fleet_name_is_reserved() {
        let file = crate::parse("apiVersion: balerix/v1\nkind: Fleet\nname: balerix\n").unwrap();
        let err = resolve(&file, &ResolveOptions::default()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "name: \"balerix\" is reserved for the daemon's plugins"
        );
        let file = crate::parse("apiVersion: balerix/v1\nkind: Fleet\nname: ok\n").unwrap();
        let err = resolve(
            &file,
            &ResolveOptions {
                name_override: Some("balerix".into()),
                ..ResolveOptions::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().starts_with("name: \"balerix\" is reserved"));
    }
```

If `ResolveOptions` does not derive `Default`, add `#[derive(Default)]` to it (its two fields are `Option`s and a `Value`; give `host_claude_settings` a `Default` via `serde_json::Value::Null` if the type is `Value`, or use `..` with an explicit construction matching the struct — read `resolve.rs:17-27` and construct accordingly).

Run: `mise x -- cargo test -p balerix-config reserved`
Expected: FAIL (no such error).

In `resolve()` right after `name` is determined (`resolve.rs:29-36`), add:

```rust
    if balerix_core::is_reserved_fleet(&name) {
        return Err(ConfigError::Invalid {
            path: "name".to_string(),
            message: format!("{name:?} is reserved for the daemon's plugins"),
        });
    }
```

Run: `mise x -- cargo test -p balerix-config`
Expected: PASS.

- [ ] **Step 9: Run the workspace check and commit**

Run: `mise run check`
Expected: PASS. `cargo mutants` is not run here; the reconciler was not touched.

```bash
git add -A crates/balerix-core crates/balerix-config crates/balerix-runtime/src/materializer.rs
git commit -m "$(cat <<'EOF'
Add ResolvedPlugin, manifest validation and the reserved balerix fleet

Plugins spec §3 and §5.2. plugin_fleet() renders the plugin list as a
synthetic Fleet (balerix/plugins/<name>) whose agents carry only the
plugin hash, so the unchanged reconciler decides restarts for plugins
exactly as it does for agents. The Materializer port gains
materialize_plugin and purge_plugin; the fake records both. The runtime
implementation is a placeholder until the next task. is_exact_version
moves to core so manifest validation and fleet validation share it, and
`balerix` is rejected as a user fleet name client-side.

toml is added to balerix-core: manifest validation reads the package's
mise.toml (exact pins, the start task) without any I/O.

Claude-Session: https://claude.ai/code/session_01AMxZmNaRYWjQoNLWBsL6m6
EOF
)"
```

---

### Task 3: `balerix-runtime` — materialize a plugin: layout, env, grants, launch, install

**Files:**
- Create: `crates/balerix-runtime/src/plugin.rs`, `crates/balerix-runtime/tests/plugin_golden.rs`, `crates/balerix-runtime/tests/plugin_it.rs`
- Modify: `crates/balerix-runtime/src/layout.rs`, `crates/balerix-runtime/src/sandbox.rs:805` (`SYSTEM_READ` → `pub(crate)`; `write_profile`/`validate_profile` take paths), `crates/balerix-runtime/src/materializer.rs` (replace the Task 2 placeholders), `crates/balerix-runtime/src/lib.rs`

**Interfaces:**
- Consumes: `balerix_core::{ResolvedPlugin, HookTarget, LaunchPlan, MaterializeError, AgentName}`; `balerix_runtime::{Grants, render_profile, merge_profile, sh_quote, outer_path, Cmd, write_atomic, ensure_dir, ensure_private_dir}`.
- Produces:
  - `StateLayout::plugins_state_dir() -> PathBuf` (`state_root/plugins`), `plugins_data_dir() -> PathBuf` (`data_root/plugins`), `plugin(&AgentName) -> PluginPaths`.
  - `pub struct PluginPaths { pub root, pub home, pub nono_home, pub kv, pub scratch, pub profile, pub launch, pub logs: PathBuf }` with `installed_marker()`, `tmp_dir()`, `xdg_config()`, `xdg_data()`, `xdg_state()`, `xdg_cache()`, `mise_config_dir()`, `mise_state_dir()`, `mise_cache_dir()`.
  - `balerix_runtime::plugin::{plugin_env, plugin_grants, render_plugin_launch, write_plugin_home, install_plugin_tools}`; `Runtime::render_plugin(&self, &ResolvedPlugin, &HookTarget) -> Result<RenderOutcome, MaterializeError>`.
  - `Runtime` implements `materialize_plugin` and `purge_plugin` for real.
  - `sandbox::write_profile_at(id: &AgentId, path: &Path, profile: &Value) -> Result<bool, MaterializeError>` and `sandbox::validate_profile_at(tools: &ToolPaths, id: &AgentId, profile: &Path, nono_home: &Path, log: &Path) -> Result<(), MaterializeError>`; the existing `write_profile`/`validate_profile` delegate to them.

- [ ] **Step 1: Write the failing layout tests**

In `crates/balerix-runtime/src/layout.rs` tests add:

```rust
    #[test]
    fn plugin_paths_live_under_plugins_in_state_and_packages_under_data() {
        let l = StateLayout::from_env(Path::new("/h"), no_env);
        assert_eq!(
            l.plugins_state_dir(),
            PathBuf::from("/h/.local/state/balerix/plugins")
        );
        assert_eq!(
            l.plugins_data_dir(),
            PathBuf::from("/h/.local/share/balerix/plugins")
        );
        let p = l.plugin(&"web".parse().unwrap());
        let base = "/h/.local/state/balerix/plugins/web";
        assert_eq!(p.root, PathBuf::from(base));
        assert_eq!(p.home, PathBuf::from(format!("{base}/home")));
        assert_eq!(p.nono_home, PathBuf::from(format!("{base}/nono")));
        assert_eq!(p.kv, PathBuf::from(format!("{base}/kv")));
        assert_eq!(p.scratch, PathBuf::from(format!("{base}/scratch")));
        assert_eq!(p.profile, PathBuf::from(format!("{base}/nono-profile.json")));
        assert_eq!(p.launch, PathBuf::from(format!("{base}/launch.sh")));
        assert_eq!(p.logs, PathBuf::from(format!("{base}/logs")));
        assert_eq!(p.installed_marker(), PathBuf::from(format!("{base}/.installed")));
        assert_eq!(p.tmp_dir(), PathBuf::from(format!("{base}/home/tmp")));
        assert_eq!(p.xdg_config(), PathBuf::from(format!("{base}/home/.config")));
        assert_eq!(
            p.mise_state_dir(),
            PathBuf::from(format!("{base}/home/.local/state/mise"))
        );
    }
```

Run: `mise x -- cargo test -p balerix-runtime layout`
Expected: FAIL to compile.

- [ ] **Step 2: Add `PluginPaths`**

In `crates/balerix-runtime/src/layout.rs` add `use balerix_core::AgentName;` to the import and:

```rust
/// Where one plugin lives (plugins spec §5.1). The package itself is under
/// `plugins_data_dir()` or wherever a directory source points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPaths {
    pub root: PathBuf,
    pub home: PathBuf,
    /// nono's own `$HOME`, as for agents (P2-5).
    pub nono_home: PathBuf,
    /// Key/value store (Phase 2 of Spec B); never granted to the sandbox.
    pub kv: PathBuf,
    /// Bulk state, read-write inside the sandbox; `BALERIX_PLUGIN_SCRATCH`.
    pub scratch: PathBuf,
    pub profile: PathBuf,
    pub launch: PathBuf,
    pub logs: PathBuf,
}

impl StateLayout {
    pub fn plugins_state_dir(&self) -> PathBuf {
        self.state_root.join("plugins")
    }
    /// Unpacked packages: `plugins/<name>/<digest12>/`.
    pub fn plugins_data_dir(&self) -> PathBuf {
        self.data_root.join("plugins")
    }
    pub fn plugin(&self, name: &AgentName) -> PluginPaths {
        let root = self.plugins_state_dir().join(name.as_str());
        PluginPaths {
            home: root.join("home"),
            nono_home: root.join("nono"),
            kv: root.join("kv"),
            scratch: root.join("scratch"),
            profile: root.join("nono-profile.json"),
            launch: root.join("launch.sh"),
            logs: root.join("logs"),
            root,
        }
    }
}

impl PluginPaths {
    /// Holds the plugin hash last installed and validated; removed when the
    /// profile changes, rewritten after a successful install.
    pub fn installed_marker(&self) -> PathBuf {
        self.root.join(".installed")
    }
    pub fn tmp_dir(&self) -> PathBuf {
        self.home.join("tmp")
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
```

Export `PluginPaths` from `lib.rs` (`pub use layout::{AgentPaths, CrewPaths, PluginPaths, StateLayout};`).

Run: `mise x -- cargo test -p balerix-runtime layout`
Expected: PASS.

- [ ] **Step 3: Write the failing unit tests for `plugin.rs`**

Create `crates/balerix-runtime/src/plugin.rs`:

```rust
//! Materializing a plugin (plugins spec §5.1): the sandboxed home, the
//! profile, `launch.sh` running `nono run → mise run <start>` from the
//! package root, and the daemon-side `mise install`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::StateLayout;
    use balerix_api::PluginManifest;
    use serde_json::json;
    use std::path::Path;

    fn plugin() -> ResolvedPlugin {
        let manifest: PluginManifest = serde_json::from_value(json!({
            "apiVersion": "balerix/v1", "kind": "Plugin", "name": "web",
            "version": "0.1.0", "protocol": 1, "start": "serve",
            "sandbox": { "network": { "block": true } }
        }))
        .unwrap();
        ResolvedPlugin {
            name: "web".parse().unwrap(),
            package: "/data/plugins/web/abc123def456".into(),
            manifest,
            config: json!({}),
            digest: Some("abc123def456ffff".into()),
        }
    }

    fn tools() -> ToolPaths {
        ToolPaths {
            git: "/usr/bin/git".into(),
            gh: "/opt/gh".into(),
            mise: "/opt/mise/bin/mise".into(),
            nono: "/opt/nono".into(),
            tmux: "/opt/tmux".into(),
            balerix: "/opt/balerix".into(),
        }
    }

    #[test]
    fn env_isolates_home_points_mise_at_the_package_and_carries_the_token() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let p = plugin();
        let paths = layout.plugin(&p.name);
        let env = plugin_env(&p, &paths, &layout, "http://127.0.0.1:7643", "tok-1");
        let base = "/h/.local/state/balerix/plugins/web";
        assert_eq!(env["HOME"], format!("{base}/home"));
        assert_eq!(env["TMPDIR"], format!("{base}/home/tmp"));
        assert_eq!(env["XDG_STATE_HOME"], format!("{base}/home/.local/state"));
        assert_eq!(
            env["MISE_GLOBAL_CONFIG_FILE"],
            "/data/plugins/web/abc123def456/mise.toml"
        );
        assert_eq!(env["MISE_CEILING_PATHS"], "/data/plugins/web/abc123def456");
        assert_eq!(env["MISE_DATA_DIR"], "/h/.local/share/balerix/mise");
        assert_eq!(env["MISE_AUTO_INSTALL"], "false");
        assert_eq!(env["BALERIX_API_URL"], "http://127.0.0.1:7643");
        assert_eq!(env["BALERIX_PLUGIN_NAME"], "web");
        assert_eq!(env["BALERIX_PLUGIN_TOKEN"], "tok-1");
        assert_eq!(env["BALERIX_PLUGIN_SCRATCH"], format!("{base}/scratch"));
        assert!(!env.contains_key("PATH"), "PATH is nono's");
        assert!(!env.contains_key("CLAUDE_CONFIG_DIR"), "no claude here");
        assert_eq!(env.len(), 17);
    }

    #[test]
    fn grants_read_the_package_and_write_only_home_and_scratch() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let p = plugin();
        let paths = layout.plugin(&p.name);
        let g = plugin_grants(&p, &paths, &layout, Path::new("/opt/balerix"), Path::new("/opt/mise"));
        assert_eq!(g.read[0], Path::new("/usr"));
        assert_eq!(g.read[5], Path::new("/h/.local/share/balerix/mise"));
        assert_eq!(g.read[6], Path::new("/data/plugins/web/abc123def456"));
        assert_eq!(g.read[7], Path::new("/opt/balerix"));
        assert_eq!(g.read[8], Path::new("/opt/mise"));
        assert_eq!(g.read.len(), 9);
        assert_eq!(
            g.allow,
            vec![paths.home.clone(), paths.scratch.clone()],
            "kv is never granted"
        );
    }

    #[test]
    fn launch_runs_the_start_task_from_the_package_root() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let p = plugin();
        let paths = layout.plugin(&p.name);
        let (script, plan) = render_plugin_launch(&p, &paths, &tools());
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains("cd '/data/plugins/web/abc123def456' && exec env -i HOME='/h/.local/state/balerix/plugins/web/nono' PATH='/usr/local/bin:/usr/bin:/bin:/opt/mise/bin'"));
        assert!(script.contains(
            "'run' '--profile' '/h/.local/state/balerix/plugins/web/nono-profile.json' '--' '/opt/mise/bin/mise' 'run' 'serve'\n"
        ));
        assert!(!script.contains("tok"), "no token input exists here");
        assert_eq!(plan.cwd, Path::new("/data/plugins/web/abc123def456"));
        assert_eq!(plan.script, paths.launch);
        assert_eq!(plan.argv[0], "/opt/nono");
        assert_eq!(plan.argv.last().unwrap(), "serve");
        assert_eq!(plan.env.len(), 2);
    }

    #[test]
    fn home_is_private_and_has_the_xdg_tree() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let layout = StateLayout::from_env(dir.path(), |_| None);
        let p = plugin();
        let paths = layout.plugin(&p.name);
        write_plugin_home(&p.name, &paths).unwrap();
        let mode = |q: &Path| std::fs::metadata(q).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&paths.home), 0o700);
        assert_eq!(mode(&paths.tmp_dir()), 0o700);
        assert_eq!(mode(&paths.kv), 0o700);
        for d in [
            paths.xdg_config(),
            paths.xdg_data(),
            paths.xdg_state(),
            paths.xdg_cache(),
            paths.scratch.clone(),
            paths.logs.clone(),
            paths.nono_home.clone(),
        ] {
            assert!(d.is_dir(), "{}", d.display());
        }
        write_plugin_home(&p.name, &paths).unwrap(); // idempotent
    }
}
```

Add `pub mod plugin;` to `lib.rs`.

- [ ] **Step 4: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-runtime plugin::`
Expected: FAIL to compile.

- [ ] **Step 5: Implement `plugin.rs`**

Above the tests:

```rust
use std::collections::BTreeMap;
use std::path::Path;

use balerix_core::{AgentName, HookTarget, LaunchPlan, MaterializeError, ResolvedPlugin};
use serde_json::Value;

use crate::fsutil::{ensure_dir, ensure_private_dir, write_atomic};
use crate::launch::{hooks_port, outer_path};
use crate::layout::{PluginPaths, StateLayout};
use crate::materializer::{RenderOutcome, Runtime};
use crate::quote::sh_quote;
use crate::sandbox::{
    Grants, SYSTEM_READ, render_profile, validate_profile_at, write_profile_at,
};
use crate::tools::{Cmd, ToolPaths};

/// The plugin's environment: the nono profile's `set_vars` (spec §5.1).
pub fn plugin_env(
    plugin: &ResolvedPlugin,
    paths: &PluginPaths,
    layout: &StateLayout,
    api_url: &str,
    token: &str,
) -> BTreeMap<String, String> {
    let s = |p: &Path| p.display().to_string();
    BTreeMap::from([
        ("HOME".to_string(), s(&paths.home)),
        ("XDG_CONFIG_HOME".to_string(), s(&paths.xdg_config())),
        ("XDG_DATA_HOME".to_string(), s(&paths.xdg_data())),
        ("XDG_STATE_HOME".to_string(), s(&paths.xdg_state())),
        ("XDG_CACHE_HOME".to_string(), s(&paths.xdg_cache())),
        ("TMPDIR".to_string(), s(&paths.tmp_dir())),
        // The package's own mise.toml is both the global file and, with the
        // cwd at the package root, the local config mise discovers; the
        // ceiling stops the walk there (cf. `env.rs` for agents).
        (
            "MISE_GLOBAL_CONFIG_FILE".to_string(),
            s(&plugin.package.join("mise.toml")),
        ),
        ("MISE_CEILING_PATHS".to_string(), s(&plugin.package)),
        ("MISE_DATA_DIR".to_string(), s(&layout.mise_data_dir())),
        ("MISE_CONFIG_DIR".to_string(), s(&paths.mise_config_dir())),
        ("MISE_STATE_DIR".to_string(), s(&paths.mise_state_dir())),
        ("MISE_CACHE_DIR".to_string(), s(&paths.mise_cache_dir())),
        // never install from inside the sandbox; the daemon did already
        ("MISE_AUTO_INSTALL".to_string(), "false".to_string()),
        ("BALERIX_API_URL".to_string(), api_url.to_string()),
        ("BALERIX_PLUGIN_NAME".to_string(), plugin.name.to_string()),
        ("BALERIX_PLUGIN_TOKEN".to_string(), token.to_string()),
        ("BALERIX_PLUGIN_SCRATCH".to_string(), s(&paths.scratch)),
    ])
}

/// Read: system dirs, the shared mise install dir, the package, the
/// balerix and mise binaries. Read-write: `home/` and `scratch/`. `kv/` is
/// reached through the API only, so it is never granted.
pub fn plugin_grants(
    plugin: &ResolvedPlugin,
    paths: &PluginPaths,
    layout: &StateLayout,
    balerix: &Path,
    mise: &Path,
) -> Grants {
    let mut read: Vec<std::path::PathBuf> = SYSTEM_READ.iter().map(Into::into).collect();
    read.push(layout.mise_data_dir());
    read.push(plugin.package.clone());
    read.push(balerix.to_path_buf());
    read.push(std::fs::canonicalize(mise).unwrap_or_else(|_| mise.to_path_buf()));
    Grants {
        read,
        allow: vec![paths.home.clone(), paths.scratch.clone()],
    }
}

/// `launch.sh` and the `LaunchPlan`: `cd <package> && exec env -i … nono
/// run --profile … -- mise run <start>`. No secret is an input here.
pub fn render_plugin_launch(
    plugin: &ResolvedPlugin,
    paths: &PluginPaths,
    tools: &ToolPaths,
) -> (String, LaunchPlan) {
    let env = BTreeMap::from([
        ("PATH".to_string(), outer_path(tools)),
        ("HOME".to_string(), paths.nono_home.display().to_string()),
    ]);
    let argv: Vec<String> = vec![
        tools.nono.display().to_string(),
        "-s".into(),
        "--log-file".into(),
        paths.logs.join("nono.log").display().to_string(),
        "run".into(),
        "--profile".into(),
        paths.profile.display().to_string(),
        "--".into(),
        tools.mise.display().to_string(),
        "run".into(),
        plugin.manifest.start.clone(),
    ];
    let mut script = format!(
        "#!/bin/sh\n# generated by balerix for plugin {} — safe to run by hand\ncd {} && exec env -i",
        plugin.name,
        sh_quote(&plugin.package.display().to_string())
    );
    for (k, v) in &env {
        script.push_str(&format!(" {k}={}", sh_quote(v)));
    }
    script.push_str(" \\\n ");
    for a in &argv {
        script.push(' ');
        script.push_str(&sh_quote(a));
    }
    script.push('\n');
    (
        script,
        LaunchPlan {
            cwd: plugin.package.clone(),
            env,
            argv,
            script: paths.launch.clone(),
        },
    )
}

fn io(name: &AgentName, path: &Path, e: std::io::Error) -> MaterializeError {
    MaterializeError::Io {
        id: balerix_core::plugin_id(name).to_string(),
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// `home/` (0700) with its XDG tree and 0700 `tmp/`, `kv/` (0700),
/// `scratch/`, `logs/`, nono's home. Idempotent.
pub fn write_plugin_home(name: &AgentName, paths: &PluginPaths) -> Result<(), MaterializeError> {
    for d in [&paths.home, &paths.tmp_dir(), &paths.kv] {
        ensure_private_dir(d).map_err(|e| io(name, d, e))?;
    }
    for d in [
        paths.xdg_config(),
        paths.xdg_data(),
        paths.xdg_state(),
        paths.xdg_cache(),
        paths.mise_config_dir(),
        paths.mise_state_dir(),
        paths.mise_cache_dir(),
        paths.scratch.clone(),
        paths.logs.clone(),
        paths.nono_home.clone(),
    ] {
        ensure_dir(&d).map_err(|e| io(name, &d, e))?;
    }
    Ok(())
}

/// Daemon-side `mise trust <package>/mise.toml` then `mise install`, with
/// the same `MISE_*` the sandbox will see, so the trust record lands in the
/// plugin's own mise state dir. `cwd=/` as for agents.
pub fn install_plugin_tools(
    tools: &ToolPaths,
    layout: &StateLayout,
    plugin: &ResolvedPlugin,
    paths: &PluginPaths,
) -> Result<(), MaterializeError> {
    let id = plugin.id().to_string();
    let s = |p: &Path| p.display().to_string();
    let env = BTreeMap::from([
        (
            "MISE_GLOBAL_CONFIG_FILE".to_string(),
            s(&plugin.package.join("mise.toml")),
        ),
        ("MISE_DATA_DIR".to_string(), s(&layout.mise_data_dir())),
        ("MISE_CONFIG_DIR".to_string(), s(&paths.mise_config_dir())),
        ("MISE_STATE_DIR".to_string(), s(&paths.mise_state_dir())),
        ("MISE_CACHE_DIR".to_string(), s(&paths.mise_cache_dir())),
        ("MISE_YES".to_string(), "1".to_string()),
        ("MISE_QUIET".to_string(), "1".to_string()),
        ("MISE_AUTO_INSTALL".to_string(), "false".to_string()),
    ]);
    let log = paths.logs.join("mise.toolchain.log");
    let run = |args: &[&str]| {
        Cmd::new(&tools.mise)
            .args(args.iter().copied())
            .envs(&env)
            .cwd(Path::new("/"))
            .log(&log)
            .run()
            .map(|_| ())
            .map_err(|f| MaterializeError::Tool {
                id: id.clone(),
                tool: f.tool,
                subcommand: f.subcommand,
                args: f.args,
                stderr: f.stderr,
            })
    };
    run(&["trust", &s(&plugin.package.join("mise.toml"))])?;
    run(&["install"])
}

impl Runtime {
    /// The file half: home, profile, `launch.sh`. `toolchain_changed` is
    /// true when the profile changed or the marker does not hold this
    /// plugin's hash; the caller then runs `install_plugin_tools` and
    /// `validate_profile_at`.
    pub fn render_plugin(
        &self,
        plugin: &ResolvedPlugin,
        host: &HookTarget,
    ) -> Result<RenderOutcome, MaterializeError> {
        let id = plugin.id();
        let paths = self.layout.plugin(&plugin.name);
        write_plugin_home(&plugin.name, &paths)?;
        let env = plugin_env(plugin, &paths, &self.layout, &host.url, &host.secret);
        let profile = render_profile(
            &id,
            &plugin_grants(
                plugin,
                &paths,
                &self.layout,
                &self.tools.balerix,
                &self.tools.mise,
            ),
            hooks_port(&host.url),
            &env,
            &plugin.manifest.sandbox,
        )?;
        let profile_changed = write_profile_at(&id, &paths.profile, &profile)?;
        let installed = std::fs::read_to_string(paths.installed_marker()).unwrap_or_default();
        let toolchain_changed = profile_changed || installed.trim() != plugin.hash().as_str();
        if toolchain_changed
            && let Err(e) = std::fs::remove_file(paths.installed_marker())
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(io(&plugin.name, &paths.installed_marker(), e));
        }
        let (script, plan) = render_plugin_launch(plugin, &paths, &self.tools);
        write_atomic(&paths.launch, script.as_bytes(), 0o755)
            .map_err(|e| io(&plugin.name, &paths.launch, e))?;
        Ok(RenderOutcome {
            plan,
            toolchain_changed,
        })
    }

    /// `mise install` + `nono profile validate` unless the marker holds the
    /// current hash; writes the hash on success.
    pub fn install_plugin(&self, plugin: &ResolvedPlugin) -> Result<(), MaterializeError> {
        let paths = self.layout.plugin(&plugin.name);
        let marker = paths.installed_marker();
        if std::fs::read_to_string(&marker)
            .map(|s| s.trim() == plugin.hash().as_str())
            .unwrap_or(false)
        {
            return Ok(());
        }
        install_plugin_tools(&self.tools, &self.layout, plugin, &paths)?;
        validate_profile_at(
            &self.tools,
            &plugin.id(),
            &paths.profile,
            &paths.nono_home,
            &paths.logs.join("nono.validate.log"),
        )?;
        write_atomic(&marker, plugin.hash().as_str().as_bytes(), 0o644)
            .map_err(|e| io(&plugin.name, &marker, e))
    }
}

/// Merges nothing: the manifest's `sandbox` goes through `render_profile`'s
/// conflict check like a fleet's. Kept as a named hook for Phase 2's
/// per-plugin additions.
pub fn plugin_sandbox(manifest_sandbox: &Value) -> &Value {
    manifest_sandbox
}
```

Drop `plugin_sandbox` if clippy flags it unused (it is not referenced; remove it rather than allow dead code).

In `crates/balerix-runtime/src/sandbox.rs`:
- `const SYSTEM_READ` → `pub(crate) const SYSTEM_READ`.
- Split `write_profile` into `pub fn write_profile_at(id: &AgentId, path: &Path, profile: &Value) -> Result<bool, MaterializeError>` (the existing body with `paths.profile` replaced by `path`) and keep `write_profile(id, paths, profile)` calling `write_profile_at(id, &paths.profile, profile)`.
- Split `validate_profile` into `pub fn validate_profile_at(tools: &ToolPaths, id: &AgentId, profile: &Path, nono_home: &Path, log: &Path) -> Result<(), MaterializeError>` (existing body parameterised) and keep `validate_profile(tools, id, paths)` calling it with `&paths.profile, &paths.nono_home, &paths.logs.join("nono.validate.log")`.
- Export both `_at` functions from `lib.rs` alongside the existing sandbox exports.

In `crates/balerix-runtime/src/materializer.rs` replace the two Task 2 placeholders:

```rust
    fn materialize_plugin(
        &self,
        plugin: &ResolvedPlugin,
        host: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        let out = self.render_plugin(plugin, host)?;
        self.install_plugin(plugin)?;
        Ok(out.plan)
    }

    fn purge_plugin(&self, name: &AgentName) -> Result<(), MaterializeError> {
        let root = self.layout.plugin(name).root;
        Self::rm_rf(&balerix_core::plugin_id(name).to_string(), &root)
    }
```

Add to `lib.rs`: `pub use plugin::{install_plugin_tools, plugin_env, plugin_grants, render_plugin_launch, write_plugin_home};`.

- [ ] **Step 6: Run the unit tests**

Run: `mise x -- cargo test -p balerix-runtime plugin:: && mise x -- cargo clippy -p balerix-runtime --all-targets -- -D warnings`
Expected: PASS (4 tests), clippy clean.

- [ ] **Step 7: Write the golden test**

Create `crates/balerix-runtime/tests/plugin_golden.rs`:

```rust
//! The generated plugin files for a fixed plugin, with the temp root
//! replaced by `<root>` so the snapshot is stable.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use balerix_api::PluginManifest;
use balerix_core::{HookTarget, ResolvedPlugin};
use balerix_runtime::{Runtime, StateLayout, ToolPaths};
use serde_json::json;

#[test]
fn plugin_files_match_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().display().to_string();
    let layout = StateLayout {
        state_root: dir.path().join("state"),
        data_root: dir.path().join("data"),
        config_root: dir.path().join("config"),
    };
    let tools = ToolPaths {
        git: "/usr/bin/git".into(),
        gh: "/usr/bin/gh".into(),
        mise: "/usr/local/bin/mise".into(),
        nono: "/usr/local/bin/nono".into(),
        tmux: "/usr/bin/tmux".into(),
        balerix: "/usr/local/bin/balerix".into(),
    };
    let rt = Runtime::new(layout.clone(), tools);
    let manifest: PluginManifest = serde_json::from_value(json!({
        "apiVersion": "balerix/v1", "kind": "Plugin", "name": "web", "version": "0.1.0",
        "protocol": 1, "start": "serve", "routes": true,
        "sandbox": { "network": { "block": true }, "filesystem": { "read": ["/opt/data"] } }
    }))
    .unwrap();
    let plugin = ResolvedPlugin {
        name: "web".parse().unwrap(),
        package: dir.path().join("data/plugins/web/0123456789ab"),
        manifest,
        config: json!({ "title": "t" }),
        digest: Some("0123456789abcdef".into()),
    };
    let host = HookTarget {
        url: "http://127.0.0.1:7643".into(),
        secret: "plugin-token".into(),
    };
    let out = rt.render_plugin(&plugin, &host).unwrap();
    assert!(out.toolchain_changed);
    let paths = layout.plugin(&plugin.name);
    let scrub = |s: String| s.replace(&root, "<root>");
    let profile = scrub(std::fs::read_to_string(&paths.profile).unwrap());
    let launch = scrub(std::fs::read_to_string(&paths.launch).unwrap());
    insta::assert_snapshot!("plugin_profile", profile);
    insta::assert_snapshot!("plugin_launch", launch);
    let again = rt.render_plugin(&plugin, &host).unwrap();
    assert!(!again.toolchain_changed, "same inputs: no reinstall");
    assert_eq!(again.plan, out.plan);
}
```

Run: `mise x -- cargo test -p balerix-runtime --test plugin_golden`
Expected: FAIL with two new `.snap.new` files. Read them and check: the profile has `meta.name` `balerix-balerix-plugins-web`; `filesystem.read` lists `/usr`, `/lib`, `/lib64`, `/bin`, `/etc`, `<root>/data/mise`, `<root>/data/plugins/web/0123456789ab`, `/usr/local/bin/balerix`, `/usr/local/bin/mise`, then `/opt/data` last (the user's read grant appended); `filesystem.allow` is exactly `<root>/state/plugins/web/home` and `<root>/state/plugins/web/scratch`; `network` is `{ "block": true, "open_port": [7643] }`; `environment.deny_vars` is `["*"]` and `set_vars` has the 17 entries of Step 3 including `"BALERIX_PLUGIN_TOKEN": "plugin-token"` (the profile is the one place the token is allowed) and `"MISE_GLOBAL_CONFIG_FILE": "<root>/data/plugins/web/0123456789ab/mise.toml"`. The launch script is the Step 3 shape with `<root>` paths and ends in `'/usr/local/bin/mise' 'run' 'serve'`. Then `mise x -- cargo insta accept` and rerun: PASS.

- [ ] **Step 8: Write the integration test against real mise and nono**

Create `crates/balerix-runtime/tests/plugin_it.rs`:

```rust
//! A real package launched under nono through `mise run` (plugins spec
//! §11 "runtime integration"): the start task runs, sees the token and its
//! scratch dir, and cannot write into the package.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::fs;
use std::process::Command;

use balerix_api::PluginManifest;
use balerix_core::{HookTarget, Materializer, ResolvedPlugin};
use balerix_runtime::Runtime;

const MANIFEST: &str = "apiVersion: balerix/v1\nkind: Plugin\nname: probe\nversion: 0.0.1\nprotocol: 1\nstart: serve\n";
const MISE_TOML: &str = r#"[tools]

[tasks.serve]
run = 'printf "%s\n%s\n%s\n" "$BALERIX_PLUGIN_NAME" "$BALERIX_PLUGIN_TOKEN" "$HOME" > "$BALERIX_PLUGIN_SCRATCH/ran"; if touch "$PWD/escape" 2>/dev/null; then echo escaped >> "$BALERIX_PLUGIN_SCRATCH/ran"; fi'
"#;

#[test]
fn a_package_runs_its_start_task_inside_the_sandbox() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise+nono", false));
        return;
    };
    let root = support::temp_root("plugin");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    fs::create_dir_all(&layout.config_root).unwrap();
    let package = root.join("pkg");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("balerix-plugin.yaml"), MANIFEST).unwrap();
    fs::write(package.join("mise.toml"), MISE_TOML).unwrap();
    let manifest: PluginManifest = serde_norway::from_str(MANIFEST).unwrap();
    let plugin = ResolvedPlugin {
        name: "probe".parse().unwrap(),
        package: package.clone(),
        manifest,
        config: serde_json::json!({}),
        digest: None,
    };
    let host = HookTarget {
        url: "http://127.0.0.1:1".into(),
        secret: "tok-secret".into(),
    };
    let rt = Runtime::new(layout.clone(), tools);
    let plan = rt.materialize_plugin(&plugin, &host).unwrap();
    let paths = layout.plugin(&plugin.name);
    assert!(paths.installed_marker().exists());
    let launch = fs::read_to_string(&paths.launch).unwrap();
    assert!(!launch.contains("tok-secret"), "token in launch.sh");

    let out = Command::new("/bin/sh")
        .arg(&plan.script)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "launch.sh failed: {}\nnono.log: {}",
        String::from_utf8_lossy(&out.stderr),
        fs::read_to_string(paths.logs.join("nono.log")).unwrap_or_default()
    );
    let ran = fs::read_to_string(paths.scratch.join("ran")).unwrap();
    let lines: Vec<&str> = ran.lines().collect();
    assert_eq!(lines[0], "probe");
    assert_eq!(lines[1], "tok-secret", "the token reaches the plugin");
    assert_eq!(lines[2], paths.home.display().to_string(), "HOME is the plugin home");
    assert_eq!(lines.len(), 3, "the package must be read-only: {ran}");
    assert!(!package.join("escape").exists());

    rt.purge_plugin(&plugin.name).unwrap();
    assert!(!paths.root.exists());
    rt.purge_plugin(&plugin.name).unwrap(); // idempotent
}
```

`serde_norway` is already a dependency of `balerix-runtime`.

Run: `mise run test-it`
Expected: PASS (or a printed skip locally without nono; CI fails instead). If `mise run` inside the sandbox fails with a trust error, the trust record is not where the sandbox looks: check that `MISE_STATE_DIR` in `install_plugin_tools` and in `plugin_env` are the same path (they must both be `paths.mise_state_dir()`).

- [ ] **Step 9: Run the workspace check and commit**

Run: `mise run check`
Expected: PASS.

```bash
git add -A crates/balerix-runtime
git commit -m "$(cat <<'EOF'
Materialize plugins under the state root and launch them via mise run

Plugins spec §5.1. A plugin gets the agent treatment: a 0700 home with its
own XDG tree and tmp, a nono profile whose read grants are the system
dirs, the shared mise dir, the package and the two binaries, read-write on
home and scratch only, and a launch.sh that cds into the package and runs
`nono run → mise run <start>`. The package's own mise.toml is the global
config, so one `mise trust` covers both the daemon-side install and the
sandboxed run. The install marker stores the plugin hash, so a changed
package, manifest or config reinstalls. The integration test runs a real
task under nono and checks the token arrives and the package stays
read-only.

Claude-Session: https://claude.ai/code/session_01AMxZmNaRYWjQoNLWBsL6m6
EOF
)"
```

---

### Task 4: `balerix-server` — `plugins.yaml`, manifests, packages, `PluginMaterializer`

**Files:**
- Create: `crates/balerix-server/src/plugins/mod.rs`, `config.rs`, `manifest.rs`, `package.rs`, `materializer.rs`
- Modify: `Cargo.toml` (workspace deps), `crates/balerix-server/Cargo.toml`, `crates/balerix-server/src/lib.rs`

**Interfaces:**
- Consumes: Task 1 `PluginsFile`, `PluginEntry`, `PluginManifest`; Task 2 `ResolvedPlugin`, `validate_manifest`, `ManifestError`, `plugin_id`, `Materializer::{materialize_plugin, purge_plugin}`.
- Produces (used by Task 5):
  - `balerix_server::plugins::PluginError` (below).
  - `config::{load_plugins_file(path: &Path) -> Result<PluginsFile, PluginError>, Source, resolve_source(entry: &PluginEntry, base: &Path) -> Result<Source, PluginError>}`.
  - `manifest::read_manifest(package: &Path) -> Result<PluginManifest, PluginError>`.
  - `package::{sha256_hex(&[u8]) -> String, digest_prefix(&str) -> &str, fetch(&str) -> Result<Vec<u8>, PluginError>, unpack(&[u8], &Path) -> Result<(), PluginError>, create(dir: &Path, out: &Path) -> Result<String, PluginError>, install(name: &str, source: &Source, expected: Option<&str>, install_root: &Path) -> Result<(PathBuf, Option<String>), PluginError>}`.
  - `materializer::{PluginMaterializer, NullStore}`: `PluginMaterializer::new(inner: Arc<dyn Materializer>) -> Self`, `replace(&self, plugins: Vec<ResolvedPlugin>)`, `get(&self, name: &AgentName) -> Option<ResolvedPlugin>`, `all(&self) -> Vec<ResolvedPlugin>`.

- [ ] **Step 1: Dependencies**

`Cargo.toml` `[workspace.dependencies]` add after `toml`:

```toml
tar = "0.4.46"
flate2 = "1.1.10"
```

`crates/balerix-server/Cargo.toml` `[dependencies]` add `serde_norway`, `sha2`, `tar`, `flate2`, `ureq` (all `{ workspace = true }`) and remove `ureq` from `[dev-dependencies]`.

- [ ] **Step 2: `mod.rs` and the error type**

Create `crates/balerix-server/src/plugins/mod.rs`:

```rust
//! Plugins in the daemon (plugins spec §2.1, §5.2): the declarative file,
//! package install, and the host that drives them as the `balerix` fleet.

pub mod config;
pub mod host;
pub mod manifest;
pub mod materializer;
pub mod package;

use std::path::PathBuf;

pub use config::{Source, load_plugins_file, resolve_source};
pub use host::{PluginHost, PluginHostConfig};
pub use manifest::read_manifest;
pub use materializer::{NullStore, PluginMaterializer};

/// Every plugin failure, with the config path or file first.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginError {
    #[error("plugins.yaml: {path}: {message}")]
    Config { path: String, message: String },
    #[error(transparent)]
    Manifest(#[from] balerix_core::ManifestError),
    #[error("balerix-plugin.yaml: {0}")]
    ManifestParse(String),
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("{source}: {message}")]
    Fetch { source: String, message: String },
    #[error("package: {0}")]
    Package(String),
    #[error("digest mismatch (expected {expected}, got {got})")]
    Digest { expected: String, got: String },
    #[error("plugin {0:?} is still declared in plugins.yaml; remove it first")]
    StillDeclared(String),
    #[error("{0}")]
    Internal(String),
}

impl PluginError {
    pub(crate) fn io(path: &std::path::Path, e: impl std::fmt::Display) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }
}
```

Until Task 5 creates `host.rs`, comment out `pub mod host;` and its re-export; Task 5 restores them.

- [ ] **Step 3: Write the failing tests for `config.rs`**

Create `crates/balerix-server/src/plugins/config.rs`:

```rust
//! `plugins.yaml` (plugins spec §2.1): parse, validate, resolve sources.

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, text: &str) -> PathBuf {
        let p = dir.join("plugins.yaml");
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn a_missing_file_is_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let f = load_plugins_file(&dir.path().join("plugins.yaml")).unwrap();
        assert!(f.plugins.is_empty());
    }

    #[test]
    fn loads_and_validates_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("web")).unwrap();
        let p = write(
            dir.path(),
            "plugins:\n  - name: flow\n    source: https://x/flow.tar.gz\n    sha256: \"0000000000000000000000000000000000000000000000000000000000000000\"\n  - name: web\n    source: ./web\n    config: { title: t }\n",
        );
        let f = load_plugins_file(&p).unwrap();
        assert_eq!(f.plugins.len(), 2);
        assert_eq!(
            resolve_source(&f.plugins[0], dir.path()).unwrap(),
            Source::Url("https://x/flow.tar.gz".into())
        );
        assert_eq!(
            resolve_source(&f.plugins[1], dir.path()).unwrap(),
            Source::Directory(dir.path().join("web"))
        );
    }

    #[test]
    fn errors_name_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                "plugins:\n  - name: Web\n    source: ./web\n",
                "plugins.yaml: plugins[0].name: invalid plugin name \"Web\": contains characters other than a-z, 0-9 and '-'",
            ),
            (
                "plugins:\n  - name: a\n    source: ./a\n  - name: a\n    source: ./b\n",
                "plugins.yaml: plugins[1].name: duplicate",
            ),
            (
                "plugins:\n  - name: a\n    source: https://x/a.tar.gz\n",
                "plugins.yaml: plugins[0].sha256: required for URL and tarball sources",
            ),
            (
                "plugins:\n  - name: a\n    source: https://x/a.tar.gz\n    sha256: xyz\n",
                "plugins.yaml: plugins[0].sha256: expected 64 lowercase hex digits",
            ),
            (
                "plugins:\n  - name: a\n    source: \"\"\n",
                "plugins.yaml: plugins[0].source: must not be empty",
            ),
            (
                "plugins:\n  - name: a\n    source: http://x/a.tar.gz\n    sha256: \"0000000000000000000000000000000000000000000000000000000000000000\"\n",
                "plugins.yaml: plugins[0].source: only https:// URLs, tarball paths and directories are accepted",
            ),
        ];
        for (text, expected) in cases {
            let p = write(dir.path(), text);
            assert_eq!(load_plugins_file(&p).unwrap_err().to_string(), expected, "{text}");
        }
        let p = write(dir.path(), "plugin: []\n");
        let e = load_plugins_file(&p).unwrap_err().to_string();
        assert!(e.starts_with("plugins.yaml: "), "{e}");
        assert!(e.contains("unknown field"), "{e}");
    }

    #[test]
    fn a_missing_tarball_path_is_reported_at_resolve_time() {
        let dir = tempfile::tempdir().unwrap();
        let entry = PluginEntry {
            name: "a".into(),
            source: "./a.tar.gz".into(),
            sha256: Some("0".repeat(64)),
            config: serde_json::json!({}),
        };
        let e = resolve_source(&entry, dir.path()).unwrap_err().to_string();
        assert!(e.ends_with("a.tar.gz: not found"), "{e}");
        std::fs::write(dir.path().join("a.tar.gz"), b"").unwrap();
        assert_eq!(
            resolve_source(&entry, dir.path()).unwrap(),
            Source::Tarball(dir.path().join("a.tar.gz"))
        );
        // absolute paths are used as-is
        let abs = PluginEntry {
            source: dir.path().join("a.tar.gz").display().to_string(),
            ..entry
        };
        assert_eq!(
            resolve_source(&abs, Path::new("/elsewhere")).unwrap(),
            Source::Tarball(dir.path().join("a.tar.gz"))
        );
    }
}
```

Run: `mise x -- cargo test -p balerix-server plugins::config`
Expected: FAIL to compile.

- [ ] **Step 4: Implement `config.rs`**

```rust
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use balerix_api::{PluginEntry, PluginsFile};
use balerix_core::name::validate_name;

use super::PluginError;

/// Where a package comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Url(String),
    Tarball(PathBuf),
    /// Used in place, never copied, no digest.
    Directory(PathBuf),
}

fn entry_error(i: usize, field: &str, message: impl Into<String>) -> PluginError {
    PluginError::Config {
        path: format!("plugins[{i}].{field}"),
        message: message.into(),
    }
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Missing file → empty list. Names valid and unique; `sha256` well-formed,
/// and present unless the source is an existing directory.
pub fn load_plugins_file(path: &Path) -> Result<PluginsFile, PluginError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PluginsFile::default()),
        Err(e) => return Err(PluginError::io(path, e)),
    };
    let file: PluginsFile = serde_norway::from_str(&text).map_err(|e| PluginError::Config {
        path: "(parse)".into(),
        message: e.to_string(),
    })?;
    let base = path.parent().unwrap_or(Path::new("."));
    let mut seen = BTreeSet::new();
    for (i, entry) in file.plugins.iter().enumerate() {
        if let Err(reason) = validate_name(&entry.name) {
            return Err(entry_error(
                i,
                "name",
                format!("invalid plugin name {:?}: {reason}", entry.name),
            ));
        }
        if !seen.insert(entry.name.clone()) {
            return Err(entry_error(i, "name", "duplicate"));
        }
        if entry.source.trim().is_empty() {
            return Err(entry_error(i, "source", "must not be empty"));
        }
        if entry.source.contains("://") && !entry.source.starts_with("https://") {
            return Err(entry_error(
                i,
                "source",
                "only https:// URLs, tarball paths and directories are accepted",
            ));
        }
        let is_dir = !entry.source.starts_with("https://") && resolve_path(&entry.source, base).is_dir();
        match &entry.sha256 {
            Some(s) if !is_hex64(s) => {
                return Err(entry_error(i, "sha256", "expected 64 lowercase hex digits"));
            }
            None if !is_dir => {
                return Err(entry_error(
                    i,
                    "sha256",
                    "required for URL and tarball sources",
                ));
            }
            _ => {}
        }
    }
    Ok(file)
}

fn resolve_path(source: &str, base: &Path) -> PathBuf {
    let p = Path::new(source);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// URL, existing tarball, or existing directory; relative paths resolve
/// against the directory holding `plugins.yaml`.
pub fn resolve_source(entry: &PluginEntry, base: &Path) -> Result<Source, PluginError> {
    if entry.source.starts_with("https://") {
        return Ok(Source::Url(entry.source.clone()));
    }
    let p = resolve_path(&entry.source, base);
    if p.is_dir() {
        Ok(Source::Directory(p))
    } else if p.is_file() {
        Ok(Source::Tarball(p))
    } else {
        Err(PluginError::Io {
            path: p,
            message: "not found".into(),
        })
    }
}
```

The parse-error display is `plugins.yaml: (parse): <serde message>`; the test only checks the prefix and `unknown field`.

Run: `mise x -- cargo test -p balerix-server plugins::config`
Expected: PASS (4 tests).

- [ ] **Step 5: Write the failing tests for `manifest.rs`**

Create `crates/balerix-server/src/plugins/manifest.rs`:

```rust
//! Reading and validating a package's manifest (plugins spec §2).

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "apiVersion: balerix/v1\nkind: Plugin\nname: web\nversion: 0.1.0\nprotocol: 1\nstart: serve\n";
    const MISE: &str = "[tools]\n[tasks.serve]\nrun = \"x\"\n";

    fn package(manifest: &str, mise: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("balerix-plugin.yaml"), manifest).unwrap();
        if let Some(m) = mise {
            std::fs::write(dir.path().join("mise.toml"), m).unwrap();
        }
        dir
    }

    #[test]
    fn reads_a_valid_package() {
        let dir = package(MANIFEST, Some(MISE));
        let m = read_manifest(dir.path()).unwrap();
        assert_eq!(m.name, "web");
        assert_eq!(m.start, "serve");
    }

    #[test]
    fn missing_or_malformed_files_name_the_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_manifest(dir.path()).unwrap_err().to_string(),
            "balerix-plugin.yaml: missing"
        );
        let dir = package(MANIFEST, None);
        assert_eq!(
            read_manifest(dir.path()).unwrap_err().to_string(),
            "mise.toml: missing"
        );
        let dir = package("apiVersion: [\n", Some(MISE));
        let e = read_manifest(dir.path()).unwrap_err().to_string();
        assert!(e.starts_with("balerix-plugin.yaml: "), "{e}");
        let dir = package(&MANIFEST.replace("protocol: 1", "protocol: 9"), Some(MISE));
        assert_eq!(
            read_manifest(dir.path()).unwrap_err().to_string(),
            "balerix-plugin.yaml: protocol: this daemon speaks protocol 1, got 9"
        );
    }
}
```

Run: `mise x -- cargo test -p balerix-server plugins::manifest`
Expected: FAIL to compile.

- [ ] **Step 6: Implement `manifest.rs`**

```rust
use std::path::Path;

use balerix_api::PluginManifest;
use balerix_core::validate_manifest;

use super::PluginError;

pub const MANIFEST_FILE: &str = "balerix-plugin.yaml";
pub const MISE_FILE: &str = "mise.toml";

fn read(package: &Path, file: &'static str) -> Result<String, PluginError> {
    match std::fs::read_to_string(package.join(file)) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(match file {
            MANIFEST_FILE => PluginError::ManifestParse("missing".into()),
            _ => PluginError::Manifest(balerix_core::ManifestError::MiseToml {
                path: String::new(),
                message: "missing".into(),
            }),
        }),
        Err(e) => Err(PluginError::io(&package.join(file), e)),
    }
}

/// Parses `balerix-plugin.yaml`, reads `mise.toml`, applies the spec §2
/// rules through `balerix_core::validate_manifest`.
pub fn read_manifest(package: &Path) -> Result<PluginManifest, PluginError> {
    let text = read(package, MANIFEST_FILE)?;
    let manifest: PluginManifest =
        serde_norway::from_str(&text).map_err(|e| PluginError::ManifestParse(e.to_string()))?;
    let mise = read(package, MISE_FILE)?;
    validate_manifest(&manifest, &mise)?;
    Ok(manifest)
}
```

`ManifestError::MiseToml { path: "", … }` displays as `mise.toml: : missing`. Make `ManifestError::MiseToml`'s display skip an empty path: in `balerix-core/src/plugin.rs` change the variant's attribute to `#[error("mise.toml: {}{message}", if path.is_empty() { String::new() } else { format!("{path}: ") })]`, and use `path: String::new()` for the parse error too (Task 2 used `"(parse)"`; replace it with `String::new()` and re-run the core tests — the core test only checks the `mise.toml: ` prefix).

Run: `mise x -- cargo test -p balerix-server plugins::manifest && mise x -- cargo test -p balerix-core plugin`
Expected: PASS.

- [ ] **Step 7: Write the failing tests for `package.rs`**

Create `crates/balerix-server/src/plugins/package.rs`:

```rust
//! Package handling (plugins spec §2, §2.1): digests, deterministic
//! tarballs, safe unpacking, fetch, install.

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    fn package_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("balerix-plugin.yaml"),
            "apiVersion: balerix/v1\nkind: Plugin\nname: p\nversion: 0.0.1\nprotocol: 1\nstart: serve\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("mise.toml"), "[tasks.serve]\nrun = \"./run.sh\"\n").unwrap();
        std::fs::create_dir(dir.path().join("lib")).unwrap();
        std::fs::write(dir.path().join("lib/a.py"), "print(1)\n").unwrap();
        std::fs::write(dir.path().join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(dir.path().join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        dir
    }

    #[test]
    fn digests_are_hex_sha256() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(digest_prefix("ba7816bf8f01cfea414140de"), "ba7816bf8f01");
    }

    #[test]
    fn create_is_deterministic_and_unpack_round_trips_with_read_only_files() {
        let src = package_dir();
        let out = tempfile::tempdir().unwrap();
        let t1 = out.path().join("p1.tar.gz");
        let t2 = out.path().join("p2.tar.gz");
        let d1 = create(src.path(), &t1).unwrap();
        let d2 = create(src.path(), &t2).unwrap();
        assert_eq!(d1, d2, "same tree, same digest");
        assert_eq!(d1, sha256_hex(&std::fs::read(&t1).unwrap()));

        let dest = out.path().join("unpacked");
        unpack(&std::fs::read(&t1).unwrap(), &dest).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("lib/a.py")).unwrap(),
            "print(1)\n"
        );
        let mode = |p: &str| std::fs::metadata(dest.join(p)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode("lib/a.py"), 0o444);
        assert_eq!(mode("run.sh"), 0o555, "exec bit kept, write bits dropped");
        assert_eq!(mode("lib"), 0o755);
    }

    fn tar_with(f: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        f(&mut b);
        let raw = b.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&raw).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn unpack_rejects_escapes_links_and_odd_entries() {
        let dest = tempfile::tempdir().unwrap();
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (
                tar_with(|b| {
                    let mut h = tar::Header::new_gnu();
                    h.set_size(1);
                    h.set_mode(0o644);
                    h.set_cksum();
                    b.append_data(&mut h, "../evil", &b"x"[..]).unwrap();
                }),
                "package: entry \"../evil\": path escapes the package",
            ),
            (
                tar_with(|b| {
                    let mut h = tar::Header::new_gnu();
                    h.set_size(1);
                    h.set_mode(0o644);
                    h.set_cksum();
                    b.append_data(&mut h, "/abs", &b"x"[..]).unwrap();
                }),
                "package: entry \"/abs\": path escapes the package",
            ),
            (
                tar_with(|b| {
                    let mut h = tar::Header::new_gnu();
                    h.set_entry_type(tar::EntryType::Symlink);
                    h.set_size(0);
                    h.set_mode(0o777);
                    h.set_link_name("/etc/passwd").unwrap();
                    h.set_cksum();
                    b.append_data(&mut h, "link", &b""[..]).unwrap();
                }),
                "package: entry \"link\": symlinks, hard links and special files are not allowed",
            ),
        ];
        for (bytes, expected) in cases {
            let d = dest.path().join("x");
            let e = unpack(&bytes, &d).unwrap_err().to_string();
            assert_eq!(e, expected);
            assert!(!d.exists(), "nothing is left behind on rejection");
        }
        let e = unpack(b"not a tarball", &dest.path().join("y")).unwrap_err().to_string();
        assert!(e.starts_with("package: "), "{e}");
    }

    #[test]
    fn install_verifies_digests_and_reuses_an_existing_unpack() {
        let src = package_dir();
        let out = tempfile::tempdir().unwrap();
        let tarball = out.path().join("p.tar.gz");
        let digest = create(src.path(), &tarball).unwrap();
        let root = out.path().join("install");
        let (dir, d) = install("p", &Source::Tarball(tarball.clone()), Some(&digest), &root).unwrap();
        assert_eq!(dir, root.join("p").join(digest_prefix(&digest)));
        assert_eq!(d.as_deref(), Some(digest.as_str()));
        assert!(dir.join("mise.toml").exists());
        let marker = dir.join("lib/a.py");
        let before = std::fs::metadata(&marker).unwrap().modified().unwrap();
        let (again, _) = install("p", &Source::Tarball(tarball.clone()), Some(&digest), &root).unwrap();
        assert_eq!(again, dir);
        assert_eq!(std::fs::metadata(&marker).unwrap().modified().unwrap(), before, "reused, not rewritten");
        let e = install("p", &Source::Tarball(tarball), Some(&"0".repeat(64)), &root).unwrap_err();
        assert!(matches!(e, PluginError::Digest { .. }), "{e}");
        assert_eq!(std::fs::read_dir(root.join("p")).unwrap().count(), 1, "no temp dir left");
        let (d, none) = install("p", &Source::Directory(src.path().to_path_buf()), None, &root).unwrap();
        assert_eq!(d, std::fs::canonicalize(src.path()).unwrap());
        assert_eq!(none, None);
    }
}
```

Run: `mise x -- cargo test -p balerix-server plugins::package`
Expected: FAIL to compile.

- [ ] **Step 8: Implement `package.rs`**

```rust
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::PluginError;
use super::config::Source;

/// Largest tarball `fetch` accepts (spec §2.1).
const MAX_TARBALL: u64 = 64 << 20;
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The first twelve hex digits: the install directory name.
pub fn digest_prefix(hex: &str) -> &str {
    &hex[..hex.len().min(12)]
}

/// `https://` only, 60 s, 64 MiB cap, no TLS features beyond ureq's default.
pub fn fetch(url: &str) -> Result<Vec<u8>, PluginError> {
    let fail = |message: String| PluginError::Fetch {
        source: url.to_string(),
        message,
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .http_status_as_error(true)
        .build()
        .into();
    let mut resp = agent.get(url).call().map_err(|e| fail(e.to_string()))?;
    let mut buf = Vec::new();
    resp.body_mut()
        .as_reader()
        .take(MAX_TARBALL + 1)
        .read_to_end(&mut buf)
        .map_err(|e| fail(e.to_string()))?;
    if buf.len() as u64 > MAX_TARBALL {
        return Err(fail(format!("larger than {} MiB", MAX_TARBALL >> 20)));
    }
    Ok(buf)
}

fn relative_inside(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Unpacks a `.tar.gz` into `dest` (created; must not exist). Rejects
/// absolute paths, `..`, links and special files; files end up 0444 (0555
/// when executable), directories 0755. On any error `dest` is removed.
pub fn unpack(tarball: &[u8], dest: &Path) -> Result<(), PluginError> {
    let result = unpack_inner(tarball, dest);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(dest);
    }
    result
}

fn unpack_inner(tarball: &[u8], dest: &Path) -> Result<(), PluginError> {
    let bad = |m: String| PluginError::Package(m);
    std::fs::create_dir_all(dest).map_err(|e| PluginError::io(dest, e))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tarball));
    for entry in archive.entries().map_err(|e| bad(e.to_string()))? {
        let mut entry = entry.map_err(|e| bad(e.to_string()))?;
        let rel = entry
            .path()
            .map(|p| p.into_owned())
            .map_err(|e| bad(e.to_string()))?;
        let shown = rel.display().to_string();
        if !relative_inside(&rel) {
            return Err(bad(format!("entry {shown:?}: path escapes the package")));
        }
        let kind = entry.header().entry_type();
        let full = dest.join(&rel);
        match kind {
            tar::EntryType::Directory => {
                std::fs::create_dir_all(&full).map_err(|e| PluginError::io(&full, e))?;
            }
            tar::EntryType::Regular => {
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| PluginError::io(parent, e))?;
                }
                let mut f = std::fs::File::create(&full).map_err(|e| PluginError::io(&full, e))?;
                std::io::copy(&mut entry, &mut f).map_err(|e| PluginError::io(&full, e))?;
                let exec = entry.header().mode().map(|m| m & 0o111 != 0).unwrap_or(false);
                let mode = if exec { 0o555 } else { 0o444 };
                std::fs::set_permissions(&full, std::fs::Permissions::from_mode(mode))
                    .map_err(|e| PluginError::io(&full, e))?;
            }
            _ => {
                return Err(bad(format!(
                    "entry {shown:?}: symlinks, hard links and special files are not allowed"
                )));
            }
        }
    }
    Ok(())
}

fn walk(dir: &Path, rel: &Path, out: &mut Vec<(PathBuf, PathBuf)>) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        let full = e.path();
        let r = rel.join(&name);
        let meta = std::fs::symlink_metadata(&full)?;
        if meta.is_dir() {
            out.push((full.clone(), r.clone()));
            walk(&full, &r, out)?;
        } else if meta.is_file() {
            out.push((full, r));
        } else {
            return Err(std::io::Error::other(format!(
                "{}: symlinks and special files cannot be packaged",
                full.display()
            )));
        }
    }
    Ok(())
}

/// A deterministic `.tar.gz` of `dir` (sorted entries, mtime 0, uid/gid 0,
/// modes 0755 for directories, 0755/0644 for files by exec bit). Returns
/// the sha256 hex of the written file.
pub fn create(dir: &Path, out: &Path) -> Result<String, PluginError> {
    let mut entries = Vec::new();
    walk(dir, Path::new(""), &mut entries).map_err(|e| PluginError::io(dir, e))?;
    let gz = flate2::GzBuilder::new().mtime(0).write(Vec::new(), flate2::Compression::default());
    let mut b = tar::Builder::new(gz);
    for (full, rel) in entries {
        let meta = std::fs::metadata(&full).map_err(|e| PluginError::io(&full, e))?;
        let mut h = tar::Header::new_gnu();
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        if meta.is_dir() {
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, &rel, std::io::empty())
                .map_err(|e| PluginError::io(&full, e))?;
        } else {
            let exec = meta.permissions().mode() & 0o111 != 0;
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(meta.len());
            h.set_mode(if exec { 0o755 } else { 0o644 });
            h.set_cksum();
            let f = std::fs::File::open(&full).map_err(|e| PluginError::io(&full, e))?;
            b.append_data(&mut h, &rel, f).map_err(|e| PluginError::io(&full, e))?;
        }
    }
    let gz = b.into_inner().map_err(|e| PluginError::io(out, e))?;
    let bytes = gz.finish().map_err(|e| PluginError::io(out, e))?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| PluginError::io(parent, e))?;
    }
    std::fs::write(out, &bytes).map_err(|e| PluginError::io(out, e))?;
    Ok(sha256_hex(&bytes))
}

/// Makes the package available and returns `(package_dir, digest)`. A
/// directory is used in place. A tarball or URL is read, digest-checked
/// against `expected` *before* unpacking, and unpacked into
/// `<install_root>/<name>/<digest12>/`, reused if already there.
pub fn install(
    name: &str,
    source: &Source,
    expected: Option<&str>,
    install_root: &Path,
) -> Result<(PathBuf, Option<String>), PluginError> {
    let bytes = match source {
        Source::Directory(d) => {
            let canon = std::fs::canonicalize(d).map_err(|e| PluginError::io(d, e))?;
            return Ok((canon, None));
        }
        Source::Tarball(p) => std::fs::read(p).map_err(|e| PluginError::io(p, e))?,
        Source::Url(u) => fetch(u)?,
    };
    let digest = sha256_hex(&bytes);
    if let Some(exp) = expected
        && exp != digest
    {
        return Err(PluginError::Digest {
            expected: exp.to_string(),
            got: digest,
        });
    }
    let dest = install_root.join(name).join(digest_prefix(&digest));
    if dest.is_dir() {
        return Ok((dest, Some(digest)));
    }
    let tmp = install_root
        .join(name)
        .join(format!(".tmp-{}-{}", digest_prefix(&digest), std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    unpack(&bytes, &tmp)?;
    std::fs::rename(&tmp, &dest).map_err(|e| PluginError::io(&dest, e))?;
    Ok((dest, Some(digest)))
}
```

`hex` is already a `balerix-server` dependency. If `resp.body_mut().as_reader()` does not exist in ureq 3.4.1, use `resp.body_mut().with_config().limit(MAX_TARBALL + 1).read_to_vec()` and compare the length the same way; check `ureq::Body` docs with `mise x -- cargo doc -p ureq --open` or the source under `~/.cargo/registry`.

Run: `mise x -- cargo test -p balerix-server plugins::package`
Expected: PASS (4 tests). If the `../evil` case fails because `tar::Header::set_path` rejects the name, build that header with `h.set_path("evil").unwrap()` followed by overwriting the raw name bytes: `h.as_gnu_mut().unwrap().name[..7].copy_from_slice(b"../evil")` and `h.set_cksum()` again.

- [ ] **Step 9: Write the failing tests for `materializer.rs`**

Create `crates/balerix-server/src/plugins/materializer.rs`:

```rust
//! The synthetic-fleet bridge (plugins spec §5.2): maps agents of the
//! `balerix` fleet back to their `ResolvedPlugin` and calls the real
//! materializer's plugin methods; a `FleetStore` that stores nothing,
//! because `plugins.yaml` is the record.

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::CredentialBundle;
    use balerix_core::fakes::FakeMaterializer;
    use balerix_core::{Fleet, FleetRecord, FleetSecrets, plugin_fleet};
    use serde_json::json;

    fn plugin(name: &str) -> ResolvedPlugin {
        ResolvedPlugin {
            name: name.parse().unwrap(),
            package: format!("/pkg/{name}").into(),
            manifest: serde_json::from_value(json!({
                "apiVersion": "balerix/v1", "kind": "Plugin", "name": name,
                "version": "0.1.0", "protocol": 1, "start": "serve"
            }))
            .unwrap(),
            config: json!({}),
            digest: None,
        }
    }

    #[test]
    fn materialize_maps_the_synthetic_agent_to_its_plugin() {
        let inner = Arc::new(FakeMaterializer::default());
        let m = PluginMaterializer::new(inner.clone());
        m.replace(vec![plugin("web"), plugin("flow")]);
        assert_eq!(m.all().len(), 2);
        assert_eq!(m.get(&"web".parse().unwrap()).unwrap().package, Path::new("/pkg/web"));
        let fleet: Fleet = plugin_fleet(&m.all());
        let agents = ResolvedAgent::from_fleet(&fleet);
        let host = HookTarget {
            url: "http://127.0.0.1:1".into(),
            secret: "t".into(),
        };
        let plan = m
            .materialize(&agents[1], &CredentialBundle::default(), &host)
            .unwrap();
        assert_eq!(plan.cwd, Path::new("/pkg/web"));
        assert_eq!(inner.calls(), vec!["materialize_plugin balerix/plugins/web"]);
        // crews and removals are no-ops: nothing to clone, state kept until purge
        m.ensure_crew(
            &agents[0].id.crew_ref(),
            &agents[0].repo,
            "none",
            &agents[0].git,
            &CredentialBundle::default(),
        )
        .unwrap();
        m.remove_agent(&agents[0].id).unwrap();
        m.remove_crew(&agents[0].id.crew_ref(), Keep::default()).unwrap();
        assert_eq!(inner.calls().len(), 1, "no inner call for crews or removals");
        m.replace(vec![]);
        let e = m
            .materialize(&agents[1], &CredentialBundle::default(), &host)
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "balerix/plugins/web: plugin is no longer declared"
        );
    }

    #[test]
    fn the_null_store_keeps_nothing() {
        let s = NullStore;
        s.put(
            &FleetRecord::new(plugin_fleet(&[]).into()),
            &FleetSecrets::default(),
        )
        .unwrap();
        assert!(s.load_all().unwrap().is_empty());
        s.purge(&"balerix".parse().unwrap()).unwrap();
    }
}
```

Run: `mise x -- cargo test -p balerix-server plugins::materializer`
Expected: FAIL to compile.

- [ ] **Step 10: Implement `materializer.rs`**

```rust
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use balerix_api::{CredentialBundle, GitSettings};
use balerix_core::{
    AgentId, AgentName, CrewRef, FleetName, FleetRecord, FleetSecrets, FleetStore, HookTarget,
    Keep, LaunchPlan, MaterializeError, Materializer, RepoRef, ResolvedAgent, ResolvedPlugin,
    StoreError,
};

pub struct PluginMaterializer {
    inner: Arc<dyn Materializer>,
    plugins: RwLock<BTreeMap<AgentName, ResolvedPlugin>>,
}

impl PluginMaterializer {
    pub fn new(inner: Arc<dyn Materializer>) -> Self {
        Self {
            inner,
            plugins: RwLock::new(BTreeMap::new()),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<AgentName, ResolvedPlugin>> {
        self.plugins.read().unwrap_or_else(|e| e.into_inner())
    }

    /// The synced set; called before the `Apply` that reconciles to it.
    pub fn replace(&self, plugins: Vec<ResolvedPlugin>) {
        let mut w = self.plugins.write().unwrap_or_else(|e| e.into_inner());
        *w = plugins.into_iter().map(|p| (p.name.clone(), p)).collect();
    }

    pub fn get(&self, name: &AgentName) -> Option<ResolvedPlugin> {
        self.read().get(name).cloned()
    }

    /// Sorted by name.
    pub fn all(&self) -> Vec<ResolvedPlugin> {
        self.read().values().cloned().collect()
    }
}

impl Materializer for PluginMaterializer {
    fn ensure_crew(
        &self,
        _: &CrewRef,
        _: &RepoRef,
        _: &str,
        _: &GitSettings,
        _: &CredentialBundle,
    ) -> Result<(), MaterializeError> {
        Ok(())
    }
    fn materialize(
        &self,
        agent: &ResolvedAgent,
        _: &CredentialBundle,
        host: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        let plugin = self
            .get(&agent.id.agent)
            .ok_or_else(|| MaterializeError::Invalid {
                id: agent.id.to_string(),
                message: "plugin is no longer declared".into(),
            })?;
        self.inner.materialize_plugin(&plugin, host)
    }
    /// State survives removal from `plugins.yaml`; only `purge_plugin` deletes.
    fn remove_agent(&self, _: &AgentId) -> Result<(), MaterializeError> {
        Ok(())
    }
    fn remove_crew(&self, _: &CrewRef, _: Keep) -> Result<(), MaterializeError> {
        Ok(())
    }
    fn materialize_plugin(
        &self,
        plugin: &ResolvedPlugin,
        host: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        self.inner.materialize_plugin(plugin, host)
    }
    fn purge_plugin(&self, name: &AgentName) -> Result<(), MaterializeError> {
        self.inner.purge_plugin(name)
    }
}

/// `plugins.yaml` is the record; the in-memory registry is the state.
pub struct NullStore;

impl FleetStore for NullStore {
    fn load_all(&self) -> Result<Vec<(FleetRecord, FleetSecrets)>, StoreError> {
        Ok(Vec::new())
    }
    fn put(&self, _: &FleetRecord, _: &FleetSecrets) -> Result<(), StoreError> {
        Ok(())
    }
    fn purge(&self, _: &FleetName) -> Result<(), StoreError> {
        Ok(())
    }
}

#[allow(dead_code)]
fn _path_type_is_used_in_tests(_: &Path) {}
```

Remove the trailing `_path_type_is_used_in_tests` helper and the `Path` import if clippy is happy without them (the tests import `Path` through `use super::*`; import it inside the test module instead: `use std::path::Path;`).

Add `pub mod plugins;` to `crates/balerix-server/src/lib.rs` and `pub use plugins::PluginError;`.

- [ ] **Step 11: Run the crate tests and commit**

Run: `mise x -- cargo test -p balerix-server && mise x -- cargo clippy -p balerix-server --all-targets -- -D warnings`
Expected: PASS.

```bash
git add Cargo.toml Cargo.lock crates/balerix-server crates/balerix-core/src/plugin.rs
git commit -m "$(cat <<'EOF'
Add plugins.yaml loading, package install and the plugin materializer

Plugins spec §2.1 and §5.2. plugins.yaml is parsed and validated with
per-entry paths in its errors; sources resolve to a URL, a tarball or a
directory used in place. Packages are digest-checked before unpacking,
unpacked with every escape, link and special entry rejected, and land
read-only under <install_root>/<name>/<digest12>/; `create` writes the
deterministic tarball `balerix plugin package` will ship. The
PluginMaterializer maps the synthetic balerix/plugins/<name> agents back
to their ResolvedPlugin and forwards to the runtime's materialize_plugin;
its NullStore keeps nothing because the file is the record.

tar 0.4.46 and flate2 1.1.10 are added for the package format; sha2,
serde_norway and ureq become server dependencies (ureq was a dev-dep).

Claude-Session: https://claude.ai/code/session_01AMxZmNaRYWjQoNLWBsL6m6
EOF
)"
```

---

### Task 5: `balerix-server` — `PluginHost`, the `balerix` fleet, `hello`, sync, API routes

**Files:**
- Create: `crates/balerix-server/src/plugins/host.rs`, `crates/balerix-server/tests/plugins_it.rs`
- Modify: `crates/balerix-server/src/plugins/mod.rs` (restore `host`), `crates/balerix-server/src/daemon.rs`, `crates/balerix-server/src/api.rs`, `crates/balerix-server/src/testing.rs`, `crates/balerix-server/src/lib.rs`, `crates/balerix-server/tests/api_it.rs:103-109`

**Interfaces:**
- Consumes: Task 4's module; `actor::{spawn, FleetHandle, Msg, Ports, READY_EVENT, Shared}`; `balerix_core::{plugin_fleet, plugin_id, is_reserved_fleet, RESERVED_FLEET, ResolvedPlugin}`.
- Produces:
  - `pub struct PluginHostConfig { pub plugins_file: PathBuf, pub install_root: PathBuf }`.
  - `PluginHost::start(config, agent_ports: &Ports, shared: Shared) -> Arc<Self>`; `PluginHost::resolve(config: &PluginHostConfig) -> Result<Vec<ResolvedPlugin>, PluginError>` (blocking); `async fn sync(&self) -> Result<SyncReport, PluginError>`; `async fn hello(&self, name: &AgentName, req: HelloRequest) -> Result<HelloResponse, DaemonError>`; `fn record(&self) -> FleetRecord`; `async fn list(&self) -> Vec<PluginStatus>`; `async fn purge(&self, name: &AgentName) -> Result<(), PluginError>`; `fn handle(&self) -> &FleetHandle`.
  - `Daemon::start(ports, handler, metrics, token, existing, plugin_config: PluginHostConfig)` (new last parameter); `Daemon::plugins(&self) -> &Arc<PluginHost>`; `async fn sync_plugins(&self) -> Result<SyncReport, PluginError>`; `async fn plugin_hello(&self, name: &AgentName, token: &str, req: HelloRequest) -> Result<HelloResponse, DaemonError>`; `get(RESERVED_FLEET)` returns the plugin record; `apply`/`down` on it return `DaemonError::Invalid("name: \"balerix\" is reserved for the daemon's plugins")`; `snapshots()` includes the plugin record, `list()` excludes it.
  - Routes: `POST /v1/plugin-host/hello` (plugin bearer), `GET /v1/plugins` → `Vec<PluginStatus>`, `POST /v1/plugins/sync` → `SyncReport`, `DELETE /v1/plugins/{name}` → `{}` (admin bearer).
  - `balerix_server::testing::plugin_config_in(dir: &Path) -> PluginHostConfig`.

- [ ] **Step 1: Implement `host.rs`**

Create `crates/balerix-server/src/plugins/host.rs`:

```rust
//! The plugin host (plugins spec §5.2): one ordinary fleet actor for the
//! reserved `balerix` fleet, fed the synthetic spec from `plugins.yaml`,
//! with `hello` as its readiness event. The per-agent hook secret the actor
//! mints is the plugin's `BALERIX_PLUGIN_TOKEN`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use balerix_api::{
    AgentPhase, CredentialBundle, FleetSpec, HelloRequest, HelloResponse, PLUGIN_PROTOCOL,
    PluginStatus, SpecHash, SyncReport,
};
use balerix_core::{
    AgentName, Clock, FleetRecord, FleetSecrets, RESERVED_FLEET, ResolvedPlugin, plugin_fleet,
};
use tokio::sync::{Mutex, RwLock, oneshot};

use super::PluginError;
use super::config::{load_plugins_file, resolve_source};
use super::manifest::read_manifest;
use super::materializer::{NullStore, PluginMaterializer};
use super::package;
use crate::actor::{self, FleetHandle, Msg, Ports, READY_EVENT, Shared};
use crate::daemon::DaemonError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginHostConfig {
    /// `$XDG_CONFIG_HOME/balerix/plugins.yaml`.
    pub plugins_file: PathBuf,
    /// `$XDG_DATA_HOME/balerix/plugins`: unpacked packages.
    pub install_root: PathBuf,
}

pub struct PluginHost {
    config: PluginHostConfig,
    materializer: Arc<PluginMaterializer>,
    handle: FleetHandle,
    clock: Arc<dyn Clock>,
    listen: RwLock<BTreeMap<AgentName, String>>,
    /// One sync at a time; a second `plugin sync` waits.
    syncing: Mutex<()>,
}

impl PluginHost {
    /// Spawns the `balerix` fleet's actor with an empty spec. Nothing runs
    /// until the first `sync`.
    pub fn start(config: PluginHostConfig, agent_ports: &Ports, shared: Shared) -> Arc<Self> {
        let materializer = Arc::new(PluginMaterializer::new(agent_ports.materializer.clone()));
        let ports = Arc::new(Ports {
            materializer: materializer.clone(),
            runner: agent_ports.runner.clone(),
            clock: agent_ports.clock.clone(),
            store: Arc::new(NullStore),
            policy: agent_ports.policy.clone(),
            hook_url: agent_ports.hook_url.clone(),
            resync: agent_ports.resync,
        });
        let name = RESERVED_FLEET.parse().unwrap_or_else(|_| unreachable!());
        let record = FleetRecord::new(plugin_fleet(&[]).into());
        let handle = actor::spawn(name, record, FleetSecrets::default(), ports, shared, false);
        Arc::new(Self {
            config,
            materializer,
            handle,
            clock: agent_ports.clock.clone(),
            listen: RwLock::new(BTreeMap::new()),
            syncing: Mutex::new(()),
        })
    }

    pub fn handle(&self) -> &FleetHandle {
        &self.handle
    }

    pub fn record(&self) -> FleetRecord {
        self.handle.status.borrow().clone()
    }

    /// Reads `plugins.yaml`, installs or locates every package, validates
    /// every manifest. Blocking; the caller runs it in `spawn_blocking`.
    /// Every error carries the entry's path so the operator knows which
    /// plugin is wrong.
    pub fn resolve(config: &PluginHostConfig) -> Result<Vec<ResolvedPlugin>, PluginError> {
        let file = load_plugins_file(&config.plugins_file)?;
        let base = config
            .plugins_file
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let entry_error = |i: usize, field: &str, message: String| PluginError::Config {
            path: if field.is_empty() {
                format!("plugins[{i}]")
            } else {
                format!("plugins[{i}].{field}")
            },
            message,
        };
        let mut out = Vec::new();
        for (i, entry) in file.plugins.iter().enumerate() {
            let source =
                resolve_source(entry, &base).map_err(|e| entry_error(i, "source", e.to_string()))?;
            let (dir, digest) = package::install(
                &entry.name,
                &source,
                entry.sha256.as_deref(),
                &config.install_root,
            )
            .map_err(|e| match e {
                PluginError::Digest { expected, got } => entry_error(
                    i,
                    "sha256",
                    format!("mismatch (expected {expected}, got {got})"),
                ),
                other => entry_error(i, "source", other.to_string()),
            })?;
            let manifest = read_manifest(&dir).map_err(|e| entry_error(i, "", e.to_string()))?;
            if manifest.name != entry.name {
                return Err(entry_error(
                    i,
                    "name",
                    format!("manifest says {:?}", manifest.name),
                ));
            }
            let name: AgentName = entry
                .name
                .parse()
                .map_err(|e: balerix_core::NameError| entry_error(i, "name", e.to_string()))?;
            out.push(ResolvedPlugin {
                name,
                package: dir,
                manifest,
                config: entry.config.clone(),
                digest,
            });
        }
        Ok(out)
    }

    /// Reconciles the running set to `plugins.yaml`: resolves, swaps the
    /// materializer's map, applies the synthetic spec. The actor's pass
    /// then stops removed plugins, restarts changed ones and starts new
    /// ones. Nothing changes when `resolve` fails.
    pub async fn sync(&self) -> Result<SyncReport, PluginError> {
        let _guard = self.syncing.lock().await;
        let cfg = self.config.clone();
        let resolved = tokio::task::spawn_blocking(move || Self::resolve(&cfg))
            .await
            .map_err(|e| PluginError::Internal(format!("sync task panicked: {e}")))??;
        let before: BTreeMap<AgentName, SpecHash> = self
            .materializer
            .all()
            .iter()
            .map(|p| (p.name.clone(), p.hash()))
            .collect();
        let mut report = SyncReport::default();
        for p in &resolved {
            match before.get(&p.name) {
                Some(h) if *h == p.hash() => report.unchanged.push(p.name.to_string()),
                _ => report.installed.push(p.name.to_string()),
            }
        }
        for name in before.keys() {
            if !resolved.iter().any(|p| &p.name == name) {
                report.stopped.push(name.to_string());
            }
        }
        self.materializer.replace(resolved.clone());
        self.listen
            .write()
            .await
            .retain(|n, _| report.unchanged.iter().any(|u| u == n.as_str()));
        let spec: FleetSpec = plugin_fleet(&resolved).into();
        let (reply, rx) = oneshot::channel();
        self.handle
            .tx
            .send(Msg::Apply {
                spec,
                credentials: CredentialBundle::default(),
                reply,
            })
            .await
            .map_err(|_| PluginError::Internal("plugin actor is gone".into()))?;
        rx.await
            .map_err(|_| PluginError::Internal("plugin actor dropped the request".into()))?;
        Ok(report)
    }

    /// The plugin is up: record where it listens, hand back its config,
    /// and tell the actor — `hello` is the plugin's `SessionStart`.
    pub async fn hello(
        &self,
        name: &AgentName,
        req: HelloRequest,
    ) -> Result<HelloResponse, DaemonError> {
        let plugin = self
            .materializer
            .get(name)
            .ok_or(DaemonError::Unauthorized)?;
        if req.name != name.as_str() {
            return Err(DaemonError::Invalid(format!(
                "hello.name: {:?} does not match the token's plugin {:?}",
                req.name,
                name.as_str()
            )));
        }
        if req.protocol != PLUGIN_PROTOCOL {
            return Err(DaemonError::Invalid(format!(
                "hello.protocol: this daemon speaks protocol {PLUGIN_PROTOCOL}, got {}",
                req.protocol
            )));
        }
        let addr: SocketAddr = req.listen.parse().map_err(|_| {
            DaemonError::Invalid(format!("hello.listen: {:?} is not host:port", req.listen))
        })?;
        if !addr.ip().is_loopback() {
            return Err(DaemonError::Invalid(
                "hello.listen: must be a loopback address".into(),
            ));
        }
        self.listen
            .write()
            .await
            .insert(name.clone(), req.listen.clone());
        self.handle
            .tx
            .send(Msg::Event {
                agent: plugin.id(),
                name: READY_EVENT.into(),
                at: self.clock.now(),
            })
            .await
            .map_err(|_| DaemonError::Internal("plugin actor is gone".into()))?;
        Ok(HelloResponse {
            config: plugin.config.clone(),
        })
    }

    /// One row per declared plugin, sorted by name.
    pub async fn list(&self) -> Vec<PluginStatus> {
        let record = self.record();
        let listen = self.listen.read().await;
        self.materializer
            .all()
            .into_iter()
            .map(|p| {
                let st = record.status.agents.get(&p.id().to_string());
                PluginStatus {
                    name: p.name.to_string(),
                    version: p.manifest.version.clone(),
                    phase: st.map_or(AgentPhase::Pending, |s| s.phase),
                    listen: listen.get(&p.name).cloned(),
                    routes: p.manifest.routes,
                    message: st.map(|s| s.message.clone()).unwrap_or_default(),
                }
            })
            .collect()
    }

    /// `plugin remove --purge`: only for a plugin no longer declared.
    /// Deletes `plugins/<name>/` through the materializer and the installed
    /// packages under `install_root/<name>/`.
    pub async fn purge(&self, name: &AgentName) -> Result<(), PluginError> {
        if self.materializer.get(name).is_some() {
            return Err(PluginError::StillDeclared(name.to_string()));
        }
        let materializer = self.materializer.clone();
        let packages = self.config.install_root.join(name.as_str());
        let name = name.clone();
        tokio::task::spawn_blocking(move || {
            materializer
                .purge_plugin(&name)
                .map_err(|e| PluginError::Internal(e.to_string()))?;
            match std::fs::remove_dir_all(&packages) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(PluginError::io(&packages, e)),
            }
        })
        .await
        .map_err(|e| PluginError::Internal(format!("purge task panicked: {e}")))?
    }
}
```

`PluginMaterializer` must implement `Materializer` for `purge_plugin` to be callable as above (it does, Task 4). Restore `pub mod host;` and `pub use host::{PluginHost, PluginHostConfig};` in `plugins/mod.rs`. Export from `lib.rs`: `pub use plugins::{PluginError, PluginHost, PluginHostConfig};`.

- [ ] **Step 2: Wire the daemon**

In `crates/balerix-server/src/daemon.rs`:

- Imports: add `use balerix_api::{HelloRequest, HelloResponse, SyncReport};`, `use balerix_core::{AgentName, is_reserved_fleet, plugin_id};`, `use crate::plugins::{PluginError, PluginHost, PluginHostConfig};`.
- `Daemon` gains `plugins: Arc<PluginHost>`.
- `start` gains the last parameter `plugin_config: PluginHostConfig`; before `let ports = Arc::new(ports);` add `let plugins = PluginHost::start(plugin_config, &ports, shared.clone());` and store it in the struct.
- Add:

```rust
    pub fn plugins(&self) -> &Arc<PluginHost> {
        &self.plugins
    }

    /// Reconciles the plugin set to `plugins.yaml`; `serve` calls it once
    /// at start and fails fast on an error, `plugin sync` on demand.
    pub async fn sync_plugins(&self) -> Result<SyncReport, PluginError> {
        self.plugins.sync().await
    }

    /// `hello` authenticates with the plugin's token — the hook secret the
    /// actor minted for `balerix/plugins/<name>` — and is otherwise the
    /// plugin's `SessionStart`.
    pub async fn plugin_hello(
        &self,
        name: &AgentName,
        token: &str,
        req: HelloRequest,
    ) -> Result<HelloResponse, DaemonError> {
        if !self.verify_secret(&plugin_id(name), token).await {
            return Err(DaemonError::Unauthorized);
        }
        self.plugins.hello(name, req).await
    }

    fn reject_reserved(name: &FleetName) -> Result<(), DaemonError> {
        if is_reserved_fleet(name.as_str()) {
            return Err(DaemonError::Invalid(format!(
                "name: {:?} is reserved for the daemon's plugins",
                name.as_str()
            )));
        }
        Ok(())
    }
```

- `apply` and `down`: first line `Self::reject_reserved(name)?;`.
- `get`: first lines:

```rust
        if is_reserved_fleet(name.as_str()) {
            return Some(self.plugins.record());
        }
```

- `snapshots`: after collecting the fleets, `out.push(self.plugins.record());` (so `/metrics` gauges include `balerix_agents{fleet="balerix",crew="plugins",…}`).
- `list`: iterate `self.fleets.read().await.values().map(|h| h.status.borrow().summary()).collect()` — the plugin record is not a fleet row.

In `crates/balerix-server/src/testing.rs` add:

```rust
/// A `PluginHostConfig` under a test directory: no `plugins.yaml` yet, so
/// the first sync is a no-op.
pub fn plugin_config_in(dir: &std::path::Path) -> crate::plugins::PluginHostConfig {
    crate::plugins::PluginHostConfig {
        plugins_file: dir.join("plugins.yaml"),
        install_root: dir.join("plugins"),
    }
}
```

In `crates/balerix/src/commands/serve.rs` the workspace must keep building: import `balerix_server::PluginHostConfig` and pass a sixth argument to `Daemon::start`:

```rust
            PluginHostConfig {
                plugins_file: layout.config_root.join("plugins.yaml"),
                install_root: layout.plugins_data_dir(),
            },
```

(Task 7 adds the fail-fast `sync_plugins` call next to it.)

In `crates/balerix-server/tests/api_it.rs` the setup around line 103 calls `Daemon::start(ports, Arc::new(PassThrough), Metrics::new().unwrap(), "admin-tok".into(), Vec::new())`. Add a `tempfile::tempdir()` before it, pass `balerix_server::testing::plugin_config_in(dir.path())` as the sixth argument, and keep the `TempDir` alive by storing it in whatever struct the setup returns (add a `_plugin_dir: tempfile::TempDir` field).

- [ ] **Step 3: Add the routes**

In `crates/balerix-server/src/api.rs`:

- Imports: `use axum::http::HeaderMap; use axum::routing::delete; use balerix_api::{HelloRequest, HelloResponse, PluginStatus, SyncReport}; use balerix_core::{AgentName, NameError}; use crate::plugins::PluginError; use serde_json::{Value, json};`.
- In `router`, add to `admin` before `.route_layer(...)`:

```rust
        .route("/v1/plugins", get(list_plugins))
        .route("/v1/plugins/sync", post(sync_plugins))
        .route("/v1/plugins/{name}", delete(purge_plugin))
```

and a third router merged in:

```rust
    let plugins = Router::new()
        .route("/v1/plugin-host/hello", post(plugin_hello))
        .layer(DefaultBodyLimit::max(64 << 10));
```

(`.merge(plugins)` next to `.merge(agents)`).

- Handlers and the error mapping:

```rust
impl From<PluginError> for ApiError {
    fn from(e: PluginError) -> Self {
        let status = match &e {
            PluginError::Config { .. }
            | PluginError::Manifest(_)
            | PluginError::ManifestParse(_)
            | PluginError::Digest { .. }
            | PluginError::Package(_)
            | PluginError::StillDeclared(_) => StatusCode::BAD_REQUEST,
            PluginError::Fetch { .. } => StatusCode::BAD_GATEWAY,
            PluginError::Io { .. } | PluginError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, e.to_string())
    }
}

/// `POST /v1/plugin-host/hello`: the plugin's bearer token, verified
/// against the `balerix/plugins/<name>` secret; unknown plugin and bad
/// token answer alike.
async fn plugin_hello(
    State(state): State<AppState>,
    headers: HeaderMap,
    b: Result<Json<HelloRequest>, JsonRejection>,
) -> Result<Json<HelloResponse>, ApiError> {
    let unauthorized = || ApiError::new(StatusCode::UNAUTHORIZED, "unknown plugin or bad token");
    let Some(token) = bearer(&headers) else {
        return Err(unauthorized());
    };
    let req = b
        .map(|Json(r)| r)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    let name: AgentName = req.name.parse().map_err(|_| unauthorized())?;
    state
        .daemon
        .plugin_hello(&name, token, req)
        .await
        .map(Json)
        .map_err(|e| match e {
            DaemonError::Unauthorized => unauthorized(),
            other => other.into(),
        })
}

async fn list_plugins(State(state): State<AppState>) -> Json<Vec<PluginStatus>> {
    Json(state.daemon.plugins().list().await)
}

async fn sync_plugins(State(state): State<AppState>) -> Result<Json<SyncReport>, ApiError> {
    Ok(Json(state.daemon.sync_plugins().await?))
}

async fn purge_plugin(
    State(state): State<AppState>,
    name: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let name: AgentName = path_name(name)?
        .parse()
        .map_err(|e: NameError| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
    state.daemon.plugins().purge(&name).await?;
    Ok(Json(json!({})))
}
```

- [ ] **Step 4: Compile and run the existing suites**

Run: `mise x -- cargo test -p balerix-server`
Expected: PASS (api_it included, with the new argument).

- [ ] **Step 5: Write the plugin integration test**

Create `crates/balerix-server/tests/plugins_it.rs`:

```rust
//! The plugin host over the fakes through a real listener (plugins spec
//! §11 "server integration", phase 1 slice): sync, hello, list, the
//! reserved fleet, removal and purge.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use balerix_api::{AgentPhase, AgentSettings, CrewSpec, FleetRequest, FleetSpec, GitSettings};
use balerix_core::{AgentId, PassThrough, plugin_id};
use balerix_server::testing::{Harness, plugin_config_in};
use balerix_server::{Daemon, Metrics, router, serve};
use serde_json::{Value, json};

const MANIFEST: &str = "apiVersion: balerix/v1\nkind: Plugin\nname: hello\nversion: 0.1.0\nprotocol: 1\nstart: serve\nroutes: true\n";
const MISE: &str = "[tools]\n[tasks.serve]\nrun = \"true\"\n";

fn write_package(dir: &Path, manifest: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("balerix-plugin.yaml"), manifest).unwrap();
    fs::write(dir.join("mise.toml"), MISE).unwrap();
}

struct Api {
    base: String,
    agent: ureq::Agent,
}

impl Api {
    fn call(&self, method: &str, path: &str, token: Option<&str>, body: Option<&Value>) -> (u16, Value) {
        let url = format!("{}{path}", self.base);
        let mut req = match method {
            "GET" => self.agent.get(&url).force_send_body(),
            "POST" => self.agent.post(&url),
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
}

fn wait(mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(5), "condition not reached");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugins_sync_hello_list_and_purge() {
    let dir = tempfile::tempdir().unwrap();
    let plugins_yaml = dir.path().join("plugins.yaml");
    let package = dir.path().join("hello-pkg");
    write_package(&package, MANIFEST);
    fs::write(
        &plugins_yaml,
        "plugins:\n  - name: hello\n    source: ./hello-pkg\n    config: { greeting: hi }\n",
    )
    .unwrap();

    let h = Harness::new(Duration::from_secs(3600));
    let ports = balerix_server::Ports {
        materializer: h.materializer.clone(),
        runner: h.runner.clone(),
        clock: h.clock.clone(),
        store: h.store.clone(),
        policy: Default::default(),
        hook_url: "http://127.0.0.1:1".into(),
        resync: Duration::from_secs(3600),
    };
    let daemon = Daemon::start(
        ports,
        Arc::new(PassThrough),
        Metrics::new().unwrap(),
        "admin-tok".into(),
        Vec::new(),
        plugin_config_in(dir.path()),
    );
    let report = daemon.sync_plugins().await.unwrap();
    assert_eq!(report.installed, vec!["hello"]);
    assert!(report.stopped.is_empty() && report.unchanged.is_empty());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(listener, router(daemon.clone()), async {
        let _ = stop_rx.await;
    }));
    let api = Api {
        base,
        agent: ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into(),
    };
    let admin = Some("admin-tok");
    let id: AgentId = plugin_id(&"hello".parse().unwrap());

    // the actor's pass materialized and started the synthetic agent
    {
        let m = h.materializer.clone();
        let r = h.runner.clone();
        tokio::task::spawn_blocking(move || {
            wait(|| {
                m.calls().contains(&"materialize_plugin balerix/plugins/hello".to_string())
                    && r.calls().contains(&"ensure_agent balerix/plugins/hello".to_string())
            })
        })
        .await
        .unwrap();
    }
    let (s, rows) = api.call("GET", "/v1/plugins", admin, None);
    assert_eq!(s, 200);
    assert_eq!(rows[0]["name"], "hello");
    assert_eq!(rows[0]["version"], "0.1.0");
    assert_eq!(rows[0]["routes"], true);
    assert!(rows[0]["listen"].is_null());
    assert_eq!(rows[0]["phase"], "starting");
    let (s, _) = api.call("GET", "/v1/plugins", None, None);
    assert_eq!(s, 401, "admin token required");

    // hello: token, protocol, listen, then Ready with the config
    let token = daemon.hook_secret(&id).await.unwrap();
    let hello = |name: &str, protocol: u32, listen: &str| {
        json!({ "name": name, "version": "0.1.0", "protocol": protocol, "listen": listen })
    };
    let (s, _) = api.call("POST", "/v1/plugin-host/hello", Some("wrong"), Some(&hello("hello", 1, "127.0.0.1:4000")));
    assert_eq!(s, 401);
    let (s, _) = api.call("POST", "/v1/plugin-host/hello", None, Some(&hello("hello", 1, "127.0.0.1:4000")));
    assert_eq!(s, 401);
    let (s, body) = api.call("POST", "/v1/plugin-host/hello", Some(&token), Some(&hello("hello", 2, "127.0.0.1:4000")));
    assert_eq!(s, 400);
    assert!(body["error"].as_str().unwrap().starts_with("hello.protocol"), "{body}");
    let (s, body) = api.call("POST", "/v1/plugin-host/hello", Some(&token), Some(&hello("hello", 1, "0.0.0.0:4000")));
    assert_eq!((s, body["error"].as_str().unwrap()), (400, "hello.listen: must be a loopback address"));
    let (s, body) = api.call("POST", "/v1/plugin-host/hello", Some(&token), Some(&hello("other", 1, "127.0.0.1:4000")));
    assert_eq!(s, 401, "the body's name must be the token's plugin; unknown plugin answers like a bad token");
    let (s, body) = api.call("POST", "/v1/plugin-host/hello", Some(&token), Some(&hello("hello", 1, "127.0.0.1:4000")));
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["config"]["greeting"], "hi");
    {
        let d = daemon.clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let rows = d.plugins().list().await;
                if rows[0].phase == AgentPhase::Ready {
                    assert_eq!(rows[0].listen.as_deref(), Some("127.0.0.1:4000"));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    // the reserved fleet: readable, not writable, not listed
    let (s, rec) = api.call("GET", "/v1/fleets/balerix", admin, None);
    assert_eq!(s, 200);
    assert_eq!(rec["status"]["agents"]["balerix/plugins/hello"]["phase"], "ready");
    let (s, rows) = api.call("GET", "/v1/fleets", admin, None);
    assert_eq!((s, rows.as_array().unwrap().len()), (200, 0), "plugins are not a fleet row");
    let spec = FleetSpec {
        name: "balerix".into(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/x".into(),
                git_ref: "main".into(),
                git: GitSettings::default(),
                agents: BTreeMap::from([("a".to_string(), AgentSettings::default())]),
            },
        )]),
    };
    let req = serde_json::to_value(FleetRequest { spec, credentials: Default::default() }).unwrap();
    let (s, body) = api.call("POST", "/v1/fleets", admin, Some(&req));
    assert_eq!(s, 400);
    assert_eq!(body["error"], "name: \"balerix\" is reserved for the daemon's plugins");
    let (s, _) = api.call("DELETE", "/v1/fleets/balerix?keep_repos=false&keep_sessions=false&purge=false", admin, None);
    assert_eq!(s, 400);

    // metrics carry the plugin fleet's gauges
    let (_, metrics) = api.call("GET", "/metrics", None, None);
    assert!(
        metrics.as_str().unwrap().contains("balerix_agents{crew=\"plugins\",fleet=\"balerix\",phase=\"ready\"} 1"),
        "{metrics}"
    );

    // a broken manifest fails the sync and changes nothing
    let bad = dir.path().join("bad-pkg");
    write_package(&bad, &MANIFEST.replace("name: hello", "name: bad").replace("protocol: 1", "protocol: 7"));
    fs::write(
        &plugins_yaml,
        "plugins:\n  - name: hello\n    source: ./hello-pkg\n    config: { greeting: hi }\n  - name: bad\n    source: ./bad-pkg\n",
    )
    .unwrap();
    let (s, body) = api.call("POST", "/v1/plugins/sync", admin, None);
    assert_eq!(s, 400);
    assert_eq!(
        body["error"],
        "plugins.yaml: plugins[1]: balerix-plugin.yaml: protocol: this daemon speaks protocol 1, got 7"
    );
    assert_eq!(daemon.plugins().list().await.len(), 1, "previous set kept");

    // purge refuses while declared; removal stops; purge then deletes
    let (s, body) = api.call("DELETE", "/v1/plugins/hello", admin, None);
    assert_eq!(s, 400);
    assert_eq!(body["error"], "plugin \"hello\" is still declared in plugins.yaml; remove it first");
    fs::write(&plugins_yaml, "plugins: []\n").unwrap();
    let (s, report) = api.call("POST", "/v1/plugins/sync", admin, None);
    assert_eq!(s, 200);
    assert_eq!(report["stopped"], json!(["hello"]));
    {
        let r = h.runner.clone();
        tokio::task::spawn_blocking(move || {
            wait(|| r.calls().contains(&"stop_agent balerix/plugins/hello".to_string()))
        })
        .await
        .unwrap();
    }
    assert!(daemon.plugins().list().await.is_empty());
    assert!(
        daemon.hook_secret(&id).await.is_none(),
        "a removed plugin's token is revoked"
    );
    let (s, _) = api.call("DELETE", "/v1/plugins/hello", admin, None);
    assert_eq!(s, 200);
    assert!(h.materializer.calls().contains(&"purge_plugin hello".to_string()));
    let (s, _) = api.call("DELETE", "/v1/plugins/Nope", admin, None);
    assert_eq!(s, 400);

    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
}
```

`Harness` fields are `pub` (`testing.rs`), `balerix_server::Ports` is re-exported from `actor`. `PassThrough`'s `Default` gives `policy: Default::default()` for `ReconcilePolicy`.

Run: `mise x -- cargo test -p balerix-server --test plugins_it`
Expected: PASS. If the `stop_agent` wait fails, check that `plugin_fleet(&[])` still contains the `plugins` crew (an empty crew keeps `RemoveCrew` off the plan and `Stop` + `RemoveAgent` on it); if the `phase == "starting"` assertion races with the actor, poll `list` until the phase is `starting` before asserting the other columns.

- [ ] **Step 6: Run the workspace check and commit**

Run: `mise run check`
Expected: PASS.

```bash
git add -A crates/balerix-server
git commit -m "$(cat <<'EOF'
Run plugins as the reserved balerix fleet and turn hello into Ready

Plugins spec §5.2 and §4.1 (hello). PluginHost owns one ordinary fleet
actor for the `balerix` fleet: sync resolves plugins.yaml, swaps the
PluginMaterializer's map and applies the synthetic spec, so the unchanged
reconciler stops removed plugins, restarts changed ones and starts new
ones with the same backoff agents get. The hook secret the actor mints per
agent is the plugin's token; `POST /v1/plugin-host/hello` verifies it,
records where the plugin listens, and sends the actor a SessionStart.
`balerix` is readable through GET /v1/fleets/balerix, rejected for POST
and DELETE, and absent from the fleet list; its gauges appear in /metrics.

Claude-Session: https://claude.ai/code/session_01AMxZmNaRYWjQoNLWBsL6m6
EOF
)"
```

---

### Task 6: `balerix-plugin-sdk` — `Env` and `hello`

**Files:**
- Create: `crates/balerix-plugin-sdk/Cargo.toml`, `crates/balerix-plugin-sdk/src/lib.rs`
- Modify: `Cargo.toml` (workspace dependency entry)

**Interfaces:**
- Consumes: `balerix_api::{HelloRequest, HelloResponse, ErrorBody, PLUGIN_PROTOCOL}`.
- Produces (used by Task 7's `dev fake-plugin` and by every future plugin):
  - `pub struct Env { pub api_url: String, pub name: String, pub token: String, pub scratch: PathBuf }`; `Env::from_env(get: impl Fn(&str) -> Option<String>) -> Result<Env, SdkError>`; `Env::from_process() -> Result<Env, SdkError>`; `Debug` redacts the token.
  - `pub struct Host`; `Host::new(env: Env) -> Host`; `Host::env(&self) -> &Env`; `Host::hello(&self, version: &str, listen: &str) -> Result<HelloResponse, SdkError>`.
  - `pub enum SdkError { MissingEnv(&'static str), Transport(String), Status { status: u16, message: String } }`.

- [ ] **Step 1: Crate scaffold**

`crates/balerix-plugin-sdk/Cargo.toml`:

```toml
[package]
name = "balerix-plugin-sdk"
description = "Rust SDK for balerix plugins: environment, hello, and (later) the host protocol"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
balerix-api = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
ureq = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }

[lints]
workspace = true
```

Add `balerix-plugin-sdk = { path = "crates/balerix-plugin-sdk" }` to `[workspace.dependencies]` in the root `Cargo.toml` (the `members = ["crates/*"]` glob picks the crate up).

- [ ] **Step 2: Write the failing tests**

`crates/balerix-plugin-sdk/src/lib.rs`:

```rust
//! The plugin side of the host protocol (plugins spec §4.1, §7). Phase 1
//! ships `Env` and `hello`; observers, interceptors, actions, KV and attach
//! arrive with Phase 2. Nothing here reads the process environment except
//! `Env::from_process`, so plugins stay testable with an injected one.

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| owned.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone())
    }

    const FULL: &[(&str, &str)] = &[
        ("BALERIX_API_URL", "http://127.0.0.1:7643/"),
        ("BALERIX_PLUGIN_NAME", "web"),
        ("BALERIX_PLUGIN_TOKEN", "tok-secret"),
        ("BALERIX_PLUGIN_SCRATCH", "/s/plugins/web/scratch"),
    ];

    #[test]
    fn env_reads_the_four_variables_and_names_the_missing_one() {
        let e = Env::from_env(env_of(FULL)).unwrap();
        assert_eq!(e.api_url, "http://127.0.0.1:7643", "trailing slash trimmed");
        assert_eq!(e.name, "web");
        assert_eq!(e.token, "tok-secret");
        assert_eq!(e.scratch, std::path::Path::new("/s/plugins/web/scratch"));
        for missing in ["BALERIX_API_URL", "BALERIX_PLUGIN_NAME", "BALERIX_PLUGIN_TOKEN", "BALERIX_PLUGIN_SCRATCH"] {
            let vars: Vec<(&str, &str)> = FULL.iter().copied().filter(|(k, _)| *k != missing).collect();
            let err = Env::from_env(env_of(&vars)).unwrap_err();
            assert_eq!(err.to_string(), format!("environment: {missing} is not set"));
        }
        let dbg = format!("{e:?}");
        assert!(dbg.contains("web") && !dbg.contains("tok-secret") && dbg.contains("<redacted>"), "{dbg}");
        let host = Host::new(e);
        let dbg = format!("{host:?}");
        assert!(!dbg.contains("tok-secret"), "{dbg}");
    }

    /// One-request stub: captures the raw request, answers with a canned
    /// status and body.
    fn stub(status_line: &'static str, body: &'static str) -> (String, mpsc::Receiver<String>) {
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

    fn host_at(url: &str) -> Host {
        let vars = [
            ("BALERIX_API_URL", url),
            ("BALERIX_PLUGIN_NAME", "web"),
            ("BALERIX_PLUGIN_TOKEN", "tok-secret"),
            ("BALERIX_PLUGIN_SCRATCH", "/s"),
        ];
        Host::new(Env::from_env(env_of(&vars)).unwrap())
    }

    #[test]
    fn hello_posts_the_request_with_the_bearer_and_returns_the_config() {
        let (url, rx) = stub("200 OK", r#"{"config":{"greeting":"hi"}}"#);
        let resp = host_at(&url).hello("0.1.0", "127.0.0.1:4321").unwrap();
        assert_eq!(resp.config["greeting"], "hi");
        let raw = rx.recv().unwrap();
        assert!(raw.starts_with("POST /v1/plugin-host/hello HTTP/1.1"), "{raw}");
        assert!(raw.to_ascii_lowercase().contains("authorization: bearer tok-secret"), "{raw}");
        let body: serde_json::Value = serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body, serde_json::json!({ "name": "web", "version": "0.1.0", "protocol": 1, "listen": "127.0.0.1:4321" }));
    }

    #[test]
    fn hello_reports_statuses_and_transport_failures() {
        let (url, _rx) = stub("401 Unauthorized", r#"{"error":"unknown plugin or bad token"}"#);
        let e = host_at(&url).hello("0.1.0", "127.0.0.1:1").unwrap_err();
        assert_eq!(e.to_string(), "daemon: HTTP 401: unknown plugin or bad token");
        let e = host_at("http://127.0.0.1:1").hello("0.1.0", "127.0.0.1:1").unwrap_err();
        assert!(e.to_string().starts_with("daemon: "), "{e}");
        assert!(matches!(e, SdkError::Transport(_)));
    }
}
```

Run: `mise x -- cargo test -p balerix-plugin-sdk`
Expected: FAIL to compile.

- [ ] **Step 3: Implement the SDK**

Above the tests:

```rust
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use balerix_api::{ErrorBody, HelloRequest, HelloResponse, PLUGIN_PROTOCOL};

/// The four `BALERIX_*` variables the daemon sets through the nono profile
/// (plugins spec §5.1).
#[derive(Clone, PartialEq, Eq)]
pub struct Env {
    /// `http://127.0.0.1:<port>`, no trailing slash.
    pub api_url: String,
    pub name: String,
    pub token: String,
    pub scratch: PathBuf,
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Env")
            .field("api_url", &self.api_url)
            .field("name", &self.name)
            .field("token", &"<redacted>")
            .field("scratch", &self.scratch)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SdkError {
    #[error("environment: {0} is not set")]
    MissingEnv(&'static str),
    #[error("daemon: {0}")]
    Transport(String),
    #[error("daemon: HTTP {status}: {message}")]
    Status { status: u16, message: String },
}

impl Env {
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self, SdkError> {
        let var = |k: &'static str| {
            get(k)
                .filter(|v| !v.trim().is_empty())
                .ok_or(SdkError::MissingEnv(k))
        };
        Ok(Self {
            api_url: var("BALERIX_API_URL")?.trim().trim_end_matches('/').to_string(),
            name: var("BALERIX_PLUGIN_NAME")?,
            token: var("BALERIX_PLUGIN_TOKEN")?,
            scratch: PathBuf::from(var("BALERIX_PLUGIN_SCRATCH")?),
        })
    }

    /// The one place the SDK reads the real process environment.
    pub fn from_process() -> Result<Self, SdkError> {
        Self::from_env(|k| std::env::var(k).ok())
    }
}

/// Typed client for the daemon's plugin-host routes.
pub struct Host {
    env: Env,
    agent: ureq::Agent,
}

impl fmt::Debug for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Host").field("env", &self.env).finish()
    }
}

const TIMEOUT: Duration = Duration::from_secs(10);

impl Host {
    pub fn new(env: Env) -> Self {
        Self {
            env,
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(TIMEOUT))
                .http_status_as_error(false)
                .build()
                .into(),
        }
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    /// `POST /v1/plugin-host/hello` (plugins spec §4.1): announces the
    /// plugin's version and listen address; the daemon marks it `Ready`
    /// and answers with the daemon-level config.
    pub fn hello(&self, version: &str, listen: &str) -> Result<HelloResponse, SdkError> {
        let req = HelloRequest {
            name: self.env.name.clone(),
            version: version.to_string(),
            protocol: PLUGIN_PROTOCOL,
            listen: listen.to_string(),
        };
        let mut resp = self
            .agent
            .post(format!("{}/v1/plugin-host/hello", self.env.api_url))
            .header("Authorization", &format!("Bearer {}", self.env.token))
            .send_json(&req)
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        if !(200..300).contains(&status) {
            let message = serde_json::from_str::<ErrorBody>(&text)
                .map(|e| e.error)
                .unwrap_or(text);
            return Err(SdkError::Status { status, message });
        }
        serde_json::from_str(&text).map_err(|e| SdkError::Transport(format!("bad hello reply: {e}")))
    }
}
```

- [ ] **Step 4: Run the tests and commit**

Run: `mise x -- cargo test -p balerix-plugin-sdk && mise x -- cargo clippy -p balerix-plugin-sdk --all-targets -- -D warnings`
Expected: PASS (3 tests).

```bash
git add Cargo.toml Cargo.lock crates/balerix-plugin-sdk
git commit -m "$(cat <<'EOF'
Add balerix-plugin-sdk with Env and hello

Plugins spec §7, the phase 1 half: a plugin reads its four BALERIX_*
variables through one injectable constructor and announces itself with
hello. The token never appears in Debug output. The rest of the host
protocol arrives with Phase 2.

Claude-Session: https://claude.ai/code/session_01AMxZmNaRYWjQoNLWBsL6m6
EOF
)"
```

---

### Task 7: `balerix` binary — `plugin install|sync|list|remove|package`, `dev fake-plugin`, `serve` wiring

**Files:**
- Create: `crates/balerix/src/commands/plugin.rs`, `crates/balerix/tests/cli_plugin.rs`
- Modify: `crates/balerix/Cargo.toml`, `crates/balerix/src/cli.rs`, `crates/balerix/src/main.rs`, `crates/balerix/src/client.rs`, `crates/balerix/src/commands/mod.rs`, `crates/balerix/src/commands/dev.rs`, `crates/balerix/src/commands/serve.rs`

**Interfaces:**
- Consumes: Task 5 routes and `PluginHostConfig`; Task 4 `balerix_server::plugins::{read_manifest, package::{create, fetch, sha256_hex, unpack}}`; Task 6 `balerix_plugin_sdk::{Env, Host}`; `balerix_runtime::StateLayout::plugins_data_dir`.
- Produces:
  - CLI: `balerix plugin install <source> [--sha256 <hex>] [--api-url]`, `plugin sync [--api-url]`, `plugin list [--json] [--api-url]`, `plugin remove <name> [--purge] [--api-url]`, `plugin package <dir> [--out <file>]`, hidden `dev fake-plugin`.
  - `Client::try_connect(api_url: Option<&str>) -> Result<Option<Client>>`, `Client::plugins() -> Result<Vec<PluginStatus>>`, `Client::sync_plugins() -> Result<SyncReport>`, `Client::purge_plugin(name: &str) -> Result<()>`.
  - `commands::plugin::{render_plugins, render_sync, load_file, save_file, install_entry, remove_entry}`.

- [ ] **Step 1: CLI surface**

`crates/balerix/Cargo.toml` `[dependencies]` add `balerix-plugin-sdk = { workspace = true }`.

In `crates/balerix/src/cli.rs` add to `Command` after `List(ListArgs)`:

```rust
    /// Manage daemon plugins declared in $XDG_CONFIG_HOME/balerix/plugins.yaml.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
```

and the new types:

```rust
#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// Add a package (directory, tarball or https URL) to plugins.yaml and sync a running daemon.
    Install(PluginInstallArgs),
    /// Reconcile the running daemon to plugins.yaml.
    Sync(ApiOnlyArgs),
    /// List declared plugins with phase and listen address.
    List(ListArgs),
    /// Remove a plugin from plugins.yaml; --purge also deletes its state and packages.
    Remove(PluginRemoveArgs),
    /// Build a plugin tarball from a package directory and print its sha256.
    Package(PluginPackageArgs),
}

#[derive(Debug, Args)]
pub struct PluginInstallArgs {
    /// Package directory, tarball path, or https:// URL of a tarball.
    pub source: String,
    /// Expected sha256 of the tarball (required for URLs).
    #[arg(long)]
    pub sha256: Option<String>,
    #[arg(long)]
    pub api_url: Option<String>,
}

#[derive(Debug, Args)]
pub struct ApiOnlyArgs {
    #[arg(long)]
    pub api_url: Option<String>,
}

#[derive(Debug, Args)]
pub struct PluginRemoveArgs {
    pub name: String,
    /// Also delete the plugin's state directory and installed packages (needs a running daemon).
    #[arg(long)]
    pub purge: bool,
    #[arg(long)]
    pub api_url: Option<String>,
}

#[derive(Debug, Args)]
pub struct PluginPackageArgs {
    /// Package directory holding balerix-plugin.yaml and mise.toml.
    pub dir: PathBuf,
    /// Output tarball (default: ./<name>-<version>.tar.gz).
    #[arg(long)]
    pub out: Option<PathBuf>,
}
```

Add to `DevCommand`:

```rust
    /// Stand-in plugin for the e2e: binds a loopback listener, sends hello,
    /// writes the reply to $BALERIX_PLUGIN_SCRATCH/fake-plugin.hello, sleeps.
    FakePlugin,
```

In `crates/balerix/src/main.rs` import `PluginCommand` and dispatch:

```rust
        Command::Plugin { command: PluginCommand::Install(args) } => commands::plugin::install_command(&args),
        Command::Plugin { command: PluginCommand::Sync(args) } => commands::plugin::sync_command(&args),
        Command::Plugin { command: PluginCommand::List(args) } => commands::plugin::list_command(&args),
        Command::Plugin { command: PluginCommand::Remove(args) } => commands::plugin::remove_command(&args),
        Command::Plugin { command: PluginCommand::Package(args) } => commands::plugin::package_command(&args),
        Command::Dev { command: DevCommand::FakePlugin } => commands::dev::fake_plugin_command(),
```

Add `pub mod plugin;` to `commands/mod.rs`.

- [ ] **Step 2: Client methods (test first)**

In `crates/balerix/src/client.rs` tests add:

```rust
    #[test]
    fn plugin_calls_hit_the_plugin_routes() {
        let (url, rx) = crate::testutil::stub_server("200 OK", r#"{"installed":["a"],"stopped":[],"unchanged":[]}"#);
        let c = Client::new(url, "t".into());
        let r = c.sync_plugins().unwrap();
        assert_eq!(r.installed, vec!["a"]);
        let raw = rx.recv().unwrap();
        assert!(raw.starts_with("POST /v1/plugins/sync HTTP/1.1"), "{raw}");
        let (url, rx) = crate::testutil::stub_server("200 OK", "{}");
        Client::new(url, "t".into()).purge_plugin("web").unwrap();
        assert!(rx.recv().unwrap().starts_with("DELETE /v1/plugins/web HTTP/1.1"));
        let (url, rx) = crate::testutil::stub_server("200 OK", "[]");
        assert!(Client::new(url, "t".into()).plugins().unwrap().is_empty());
        assert!(rx.recv().unwrap().starts_with("GET /v1/plugins HTTP/1.1"));
    }
```

Run: `mise x -- cargo test -p balerix client::tests::plugin_calls`
Expected: FAIL to compile.

Add to `Client` (imports: `balerix_api::{PluginStatus, SyncReport}`, `serde_json::json`):

```rust
    /// `Some(client)` when an endpoint and token exist, `None` when no
    /// daemon has published an endpoint (so `plugin install` can edit the
    /// file offline). A stale endpoint still yields a client whose calls
    /// fail with `NOT_RUNNING`.
    pub fn try_connect(api_url: Option<&str>) -> Result<Option<Self>> {
        match Self::connect(api_url) {
            Ok(c) => Ok(Some(c)),
            Err(e) if e.to_string().starts_with(NOT_RUNNING) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn plugins(&self) -> Result<Vec<PluginStatus>> {
        Self::must(self.request("GET", "/v1/plugins", None), "plugin list")
    }

    pub fn sync_plugins(&self) -> Result<SyncReport> {
        Self::must(
            self.request("POST", "/v1/plugins/sync", Some(&json!({}))),
            "sync report",
        )
    }

    pub fn purge_plugin(&self, name: &str) -> Result<()> {
        Self::must(
            self.request::<Value>("DELETE", &format!("/v1/plugins/{name}"), None),
            &format!("plugin {name}"),
        )
        .map(|_| ())
    }
```

Run: `mise x -- cargo test -p balerix client`
Expected: PASS.

- [ ] **Step 3: Write the failing unit tests for `commands/plugin.rs`**

Create `crates/balerix/src/commands/plugin.rs` with the test module:

```rust
//! `balerix plugin …` (plugins spec §10): edits `plugins.yaml`, syncs a
//! running daemon, lists, purges, packages.

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::AgentPhase;

    #[test]
    fn renders_the_plugin_table_and_sync_report() {
        let rows = vec![
            PluginStatus {
                name: "flow".into(),
                version: "0.1.0".into(),
                phase: AgentPhase::Ready,
                listen: Some("127.0.0.1:4000".into()),
                routes: false,
                message: String::new(),
            },
            PluginStatus {
                name: "web".into(),
                version: "0.2.0".into(),
                phase: AgentPhase::Starting,
                listen: None,
                routes: true,
                message: "exited with status 1".into(),
            },
        ];
        assert_eq!(
            render_plugins(&rows),
            "NAME  VERSION  PHASE     LISTEN          ROUTES  MESSAGE\n\
             flow  0.1.0    ready     127.0.0.1:4000  no\n\
             web   0.2.0    starting  -               yes     exited with status 1\n"
        );
        assert_eq!(render_plugins(&[]), "no plugins\n");
        let r = SyncReport {
            installed: vec!["a".into(), "b".into()],
            stopped: vec![],
            unchanged: vec!["c".into()],
        };
        assert_eq!(render_sync(&r), "installed: a, b\nunchanged: c\n");
        assert_eq!(render_sync(&SyncReport::default()), "nothing to do\n");
    }

    #[test]
    fn file_edits_add_once_and_remove_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("balerix").join("plugins.yaml");
        let mut file = load_file(&path).unwrap();
        assert!(file.plugins.is_empty(), "missing file is empty");
        let entry = PluginEntry {
            name: "web".into(),
            source: "/pkg/web.tar.gz".into(),
            sha256: Some("ab".into()),
            config: serde_json::json!({}),
        };
        install_entry(&mut file, entry.clone()).unwrap();
        assert_eq!(
            install_entry(&mut file, entry).unwrap_err().to_string(),
            "plugin \"web\" is already declared in plugins.yaml"
        );
        save_file(&path, &file).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("name: web") && text.contains("sha256: ab"), "{text}");
        let mut again = load_file(&path).unwrap();
        assert!(remove_entry(&mut again, "web"));
        assert!(!remove_entry(&mut again, "web"));
        assert!(again.plugins.is_empty());
    }
}
```

Run: `mise x -- cargo test -p balerix commands::plugin`
Expected: FAIL to compile.

- [ ] **Step 4: Implement `commands/plugin.rs`**

```rust
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use balerix_api::{PluginEntry, PluginManifest, PluginStatus, PluginsFile, SyncReport};
use balerix_server::plugins::package::{create, fetch, sha256_hex, unpack};
use balerix_server::plugins::read_manifest;

use crate::cli::{ApiOnlyArgs, ListArgs, PluginInstallArgs, PluginPackageArgs, PluginRemoveArgs};
use crate::client::{Client, NOT_RUNNING};
use crate::wiring::layout_from_env;

const NOT_SYNCED: &str = "daemon not running; `balerix serve` syncs plugins at start\n";

fn label<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

pub fn render_plugins(rows: &[PluginStatus]) -> String {
    if rows.is_empty() {
        return "no plugins\n".to_string();
    }
    let rows: Vec<[String; 6]> = rows
        .iter()
        .map(|p| {
            [
                p.name.clone(),
                p.version.clone(),
                label(p.phase),
                p.listen.clone().unwrap_or_else(|| "-".to_string()),
                if p.routes { "yes" } else { "no" }.to_string(),
                p.message.clone(),
            ]
        })
        .collect();
    super::fleet::table(
        &["NAME", "VERSION", "PHASE", "LISTEN", "ROUTES", "MESSAGE"],
        &rows,
    )
}

pub fn render_sync(r: &SyncReport) -> String {
    let mut out = String::new();
    for (label, names) in [
        ("installed", &r.installed),
        ("stopped", &r.stopped),
        ("unchanged", &r.unchanged),
    ] {
        if !names.is_empty() {
            out.push_str(&format!("{label}: {}\n", names.join(", ")));
        }
    }
    if out.is_empty() {
        out.push_str("nothing to do\n");
    }
    out
}

pub fn plugins_file_path() -> Result<PathBuf> {
    Ok(layout_from_env()?.config_root.join("plugins.yaml"))
}

/// Missing file → empty. Not validated here: the daemon validates on sync.
pub fn load_file(path: &Path) -> Result<PluginsFile> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_norway::from_str(&text)
            .map_err(|e| anyhow!("{}: invalid plugins file: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PluginsFile::default()),
        Err(e) => Err(e).with_context(|| path.display().to_string()),
    }
}

/// Rewrites the file; comments are not preserved.
pub fn save_file(path: &Path, file: &PluginsFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| parent.display().to_string())?;
    }
    let text = serde_norway::to_string(file)?;
    std::fs::write(path, text).with_context(|| path.display().to_string())
}

pub fn install_entry(file: &mut PluginsFile, entry: PluginEntry) -> Result<()> {
    if file.plugins.iter().any(|p| p.name == entry.name) {
        bail!(
            "plugin {:?} is already declared in plugins.yaml",
            entry.name
        );
    }
    file.plugins.push(entry);
    Ok(())
}

pub fn remove_entry(file: &mut PluginsFile, name: &str) -> bool {
    let before = file.plugins.len();
    file.plugins.retain(|p| p.name != name);
    file.plugins.len() != before
}

/// Reads the manifest behind a source and decides what to record: the
/// absolute path or URL, and the digest for tarballs and URLs.
fn describe_source(
    source: &str,
    sha256: Option<&str>,
) -> Result<(String, Option<String>, PluginManifest)> {
    if source.starts_with("https://") {
        let expected = sha256.ok_or_else(|| anyhow!("--sha256 is required for a URL source"))?;
        let bytes = fetch(source)?;
        let digest = sha256_hex(&bytes);
        if digest != expected {
            bail!("digest mismatch for {source} (expected {expected}, got {digest})");
        }
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("pkg");
        unpack(&bytes, &dir)?;
        return Ok((source.to_string(), Some(digest), read_manifest(&dir)?));
    }
    if source.contains("://") {
        bail!("only https:// URLs, tarball paths and directories are accepted");
    }
    let path = std::fs::canonicalize(source).with_context(|| format!("{source}: not found"))?;
    if path.is_dir() {
        let manifest = read_manifest(&path)?;
        return Ok((path.display().to_string(), None, manifest));
    }
    let bytes = std::fs::read(&path).with_context(|| path.display().to_string())?;
    let digest = sha256_hex(&bytes);
    if let Some(expected) = sha256
        && expected != digest
    {
        bail!(
            "digest mismatch for {} (expected {expected}, got {digest})",
            path.display()
        );
    }
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("pkg");
    unpack(&bytes, &dir)?;
    Ok((path.display().to_string(), Some(digest), read_manifest(&dir)?))
}

/// Syncs when a daemon is reachable; otherwise says `serve` will.
fn sync_if_running(api_url: Option<&str>) -> Result<String> {
    match Client::try_connect(api_url)? {
        Some(c) => match c.sync_plugins() {
            Ok(r) => Ok(render_sync(&r)),
            Err(e) if e.to_string().starts_with(NOT_RUNNING) => Ok(NOT_SYNCED.to_string()),
            Err(e) => Err(e),
        },
        None => Ok(NOT_SYNCED.to_string()),
    }
}

pub fn install_command(args: &PluginInstallArgs) -> Result<String> {
    let (source, sha256, manifest) = describe_source(&args.source, args.sha256.as_deref())?;
    let path = plugins_file_path()?;
    let mut file = load_file(&path)?;
    install_entry(
        &mut file,
        PluginEntry {
            name: manifest.name.clone(),
            source,
            sha256: sha256.clone(),
            config: serde_json::Value::Object(serde_json::Map::new()),
        },
    )?;
    save_file(&path, &file)?;
    let mut out = format!(
        "declared {} {} in {}\n",
        manifest.name,
        manifest.version,
        path.display()
    );
    if let Some(d) = sha256 {
        out.push_str(&format!("sha256: {d}\n"));
    }
    out.push_str(&sync_if_running(args.api_url.as_deref())?);
    Ok(out)
}

pub fn sync_command(args: &ApiOnlyArgs) -> Result<String> {
    let client = Client::connect(args.api_url.as_deref())?;
    Ok(render_sync(&client.sync_plugins()?))
}

pub fn list_command(args: &ListArgs) -> Result<String> {
    let client = Client::connect(args.api_url.as_deref())?;
    let rows = client.plugins()?;
    Ok(if args.json {
        serde_json::to_string_pretty(&rows)? + "\n"
    } else {
        render_plugins(&rows)
    })
}

pub fn remove_command(args: &PluginRemoveArgs) -> Result<String> {
    let path = plugins_file_path()?;
    let mut file = load_file(&path)?;
    if !remove_entry(&mut file, &args.name) {
        bail!("plugin {:?} is not declared in {}", args.name, path.display());
    }
    save_file(&path, &file)?;
    if args.purge {
        let client = Client::connect(args.api_url.as_deref())
            .map_err(|e| anyhow!("{e}; --purge needs a running daemon (the entry was removed)"))?;
        let report = client.sync_plugins()?;
        client.purge_plugin(&args.name)?;
        return Ok(format!(
            "{}removed {} and purged its state\n",
            render_sync(&report),
            args.name
        ));
    }
    Ok(format!(
        "removed {} from plugins.yaml (state kept; --purge deletes it)\n{}",
        args.name,
        sync_if_running(args.api_url.as_deref())?
    ))
}

pub fn package_command(args: &PluginPackageArgs) -> Result<String> {
    let manifest = read_manifest(&args.dir)?;
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}-{}.tar.gz", manifest.name, manifest.version)));
    let digest = create(&args.dir, &out)?;
    Ok(format!("{}\nsha256: {digest}\n", out.display()))
}
```

`super::fleet::table` is private in `commands/fleet.rs`; make it `pub(crate) fn table<const N: usize>(…)`. The `balerix` crate already depends on `tempfile`, `serde_norway`, `balerix-server`.

Run: `mise x -- cargo test -p balerix commands::plugin`
Expected: PASS (2 tests).

- [ ] **Step 5: `dev fake-plugin` and `serve` wiring**

In `crates/balerix/src/commands/dev.rs` add:

```rust
/// `balerix dev fake-plugin`: the e2e's plugin. Binds a loopback listener
/// (plugins spec §11.1 row 1: can a sandboxed plugin bind at all?), says
/// hello with that address, records the reply, sleeps until killed. If the
/// bind is refused it still says hello with `127.0.0.1:0` and leaves
/// `fake-plugin.bind-failed` in scratch, so the e2e reports the verdict
/// instead of hanging.
pub fn fake_plugin_command() -> Result<String> {
    use balerix_plugin_sdk::{Env, Host};
    let env = Env::from_process()?;
    let scratch = env.scratch.clone();
    std::fs::create_dir_all(&scratch)?;
    let (listener, listen) = match std::net::TcpListener::bind("127.0.0.1:0") {
        Ok(l) => {
            let addr = l.local_addr()?.to_string();
            (Some(l), addr)
        }
        Err(e) => {
            eprintln!("fake-plugin: cannot bind a loopback listener: {e}");
            std::fs::write(scratch.join("fake-plugin.bind-failed"), e.to_string())?;
            (None, "127.0.0.1:0".to_string())
        }
    };
    let host = Host::new(env);
    let resp = host.hello(env!("CARGO_PKG_VERSION"), &listen)?;
    std::fs::write(
        scratch.join("fake-plugin.hello"),
        serde_json::to_string_pretty(&resp.config)?,
    )?;
    eprintln!("fake-plugin: hello acknowledged; listening on {listen}; sleeping until killed");
    let _keep = listener;
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
```

In `crates/balerix/src/commands/serve.rs` (the `PluginHostConfig` argument landed in Task 5): import `anyhow::anyhow` and, right after `Daemon::start` returns, before `write_endpoint`, add:

```rust
        let plugins = daemon
            .sync_plugins()
            .await
            .map_err(|e| anyhow!("plugins: {e}"))?;
        tracing::info!(
            installed = plugins.installed.len(),
            unchanged = plugins.unchanged.len(),
            "plugins synced"
        );
```

A failing sync therefore exits `serve` before the endpoint is published; the `-d` parent reports "daemon exited early; see <log>" and the log has the `plugins.yaml: …` message.

Run: `mise x -- cargo build -p balerix && mise x -- cargo run -q -p balerix -- plugin --help`
Expected: builds; help lists `install`, `sync`, `list`, `remove`, `package`.

- [ ] **Step 6: Offline CLI test**

Create `crates/balerix/tests/cli_plugin.rs`:

```rust
//! `balerix plugin package|install|remove` without a daemon, through the
//! binary, under a private HOME.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

const MANIFEST: &str = "apiVersion: balerix/v1\nkind: Plugin\nname: hello\nversion: 0.1.0\nprotocol: 1\nstart: serve\n";
const MISE: &str = "[tools]\n[tasks.serve]\nrun = \"true\"\n";

fn balerix(home: &Path) -> Command {
    let mut c = Command::cargo_bin("balerix").unwrap();
    c.env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("BALERIX_API_URL")
        .current_dir(home);
    c
}

#[test]
fn package_install_and_remove_edit_plugins_yaml_offline() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let pkg = dir.path().join("pkg");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(pkg.join("balerix-plugin.yaml"), MANIFEST).unwrap();
    fs::write(pkg.join("mise.toml"), MISE).unwrap();

    let out = balerix(&home)
        .args(["plugin", "package", &pkg.display().to_string()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.starts_with("hello-0.1.0.tar.gz\nsha256: "), "{out}");
    let digest = out.trim().rsplit(' ').next().unwrap().to_string();
    assert_eq!(digest.len(), 64);
    let tarball = home.join("hello-0.1.0.tar.gz");
    assert!(tarball.exists());

    balerix(&home)
        .args(["plugin", "install", &tarball.display().to_string(), "--sha256", &"0".repeat(64)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("digest mismatch"));

    balerix(&home)
        .args(["plugin", "install", &tarball.display().to_string()])
        .assert()
        .success()
        .stdout(predicate::str::contains("declared hello 0.1.0"))
        .stdout(predicate::str::contains(&digest))
        .stdout(predicate::str::contains("daemon not running"));
    let yaml = fs::read_to_string(home.join(".config/balerix/plugins.yaml")).unwrap();
    assert!(yaml.contains("name: hello"), "{yaml}");
    assert!(yaml.contains(&tarball.display().to_string()), "{yaml}");
    assert!(yaml.contains(&format!("sha256: {digest}")), "{yaml}");

    balerix(&home)
        .args(["plugin", "install", &pkg.display().to_string()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already declared"));

    balerix(&home)
        .args(["plugin", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("daemon not running"));

    balerix(&home)
        .args(["plugin", "remove", "hello", "--purge"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--purge needs a running daemon"));
    // the entry was removed even though the purge could not run
    let yaml = fs::read_to_string(home.join(".config/balerix/plugins.yaml")).unwrap();
    assert!(!yaml.contains("name: hello"), "{yaml}");

    balerix(&home)
        .args(["plugin", "remove", "hello"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not declared"));

    // a directory source records the absolute path and no digest
    balerix(&home)
        .args(["plugin", "install", &pkg.display().to_string()])
        .assert()
        .success();
    let yaml = fs::read_to_string(home.join(".config/balerix/plugins.yaml")).unwrap();
    assert!(yaml.contains(&fs::canonicalize(&pkg).unwrap().display().to_string()), "{yaml}");
    assert!(!yaml.contains("sha256"), "{yaml}");
}
```

Run: `mise x -- cargo test -p balerix --test cli_plugin`
Expected: PASS.

- [ ] **Step 7: Run the workspace check and commit**

Run: `mise run check`
Expected: PASS.

```bash
git add -A crates/balerix
git commit -m "$(cat <<'EOF'
Add balerix plugin install|sync|list|remove|package and dev fake-plugin

Plugins spec §10. plugins.yaml is the source of truth: install and remove
edit it (recording the absolute path or URL and the tarball digest) and
sync a running daemon, or say that `serve` will. `serve` wires the plugin
host at the XDG paths and fails fast when the file cannot be synced.
`dev fake-plugin` is the e2e's plugin: it binds a loopback listener under
nono, says hello through the SDK and records the reply.

Claude-Session: https://claude.ai/code/session_01AMxZmNaRYWjQoNLWBsL6m6
EOF
)"
```

---

### Task 8: the e2e — a plugin reaches `Ready` under nono

**Files:**
- Modify: `crates/balerix/tests/e2e.rs`

**Interfaces:**
- Consumes: everything above through the binary; the `World` helpers already in `e2e.rs` (`tool`, `require_or_skip`, `landlock_works`, `World::{balerix, run, ok, state}`).

- [ ] **Step 1: Write the journey**

Append to `crates/balerix/tests/e2e.rs`:

```rust
fn plugin_package(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join("balerix-plugin.yaml"),
        "apiVersion: balerix/v1\nkind: Plugin\nname: hello\nversion: 0.1.0\nprotocol: 1\nstart: serve\nroutes: false\n",
    )
    .unwrap();
    // an empty tool table (nothing to download) and a task that runs this
    // very binary as the plugin
    fs::write(
        dir.join("mise.toml"),
        format!("[tools]\n\n[tasks.serve]\nrun = \"'{BALERIX}' dev fake-plugin\"\n"),
    )
    .unwrap();
}

/// Plugins spec §13 item 1, "done when": a trivial SDK plugin reaches
/// `Ready` through a real `mise run` under nono, and the reserved fleet,
/// removal and purge behave.
#[test]
fn plugin_hello_journey() {
    let Some(nono) = tool("nono") else {
        assert!(!require_or_skip("nono", false));
        return;
    };
    for t in ["git", "gh", "mise", "tmux"] {
        if !require_or_skip(t, tool(t).is_some()) {
            return;
        }
    }
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("e2e-plugins-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    if !require_or_skip("landlock", landlock_works(&nono, &root)) {
        return;
    }
    let w = World {
        home: root.join("home"),
        socket: format!("balerix-e2e-plugins-{}", std::process::id()),
        tmux: tool("tmux").unwrap(),
    };
    fs::create_dir_all(&w.home).unwrap();
    let cfg = w.home.join(".config/balerix");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("mise.toml"), "[tools]\n").unwrap();
    let pkg = root.join("hello-pkg");
    plugin_package(&pkg);
    fs::write(
        cfg.join("plugins.yaml"),
        format!(
            "plugins:\n  - name: hello\n    source: \"{}\"\n    config: {{ greeting: hi }}\n",
            pkg.display()
        ),
    )
    .unwrap();

    let out = w.ok(&["serve", "-d", "--bind", "127.0.0.1:0", "--tmux-socket", &w.socket]);
    assert!(out.contains("http://127.0.0.1:"), "{out}");
    let url = fs::read_to_string(w.state().join("server/endpoint")).unwrap().trim().to_string();
    let plugin_dir = w.state().join("plugins/hello");

    // Ready through hello, with a real loopback listen address
    let start = Instant::now();
    let list = loop {
        let list = w.ok(&["plugin", "list"]);
        if list.contains("ready") {
            break list;
        }
        assert!(
            start.elapsed() < Duration::from_secs(180),
            "plugin never became ready; last list:\n{list}\nnono.log:\n{}\nmise.toolchain.log:\n{}",
            fs::read_to_string(plugin_dir.join("logs/nono.log")).unwrap_or_default(),
            fs::read_to_string(plugin_dir.join("logs/mise.toolchain.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    assert!(list.contains("hello  0.1.0    ready"), "{list}");
    assert!(list.contains("127.0.0.1:"), "{list}");
    assert!(
        !plugin_dir.join("scratch/fake-plugin.bind-failed").exists(),
        "spec §11.1 row 1 FAILED: a sandboxed plugin could not bind a loopback listener; record the verdict and take the Unix-socket fallback in phase 2: {}",
        fs::read_to_string(plugin_dir.join("scratch/fake-plugin.bind-failed")).unwrap_or_default()
    );
    let hello = fs::read_to_string(plugin_dir.join("scratch/fake-plugin.hello")).unwrap();
    assert!(hello.contains("\"greeting\": \"hi\""), "{hello}");
    let rec: FleetRecord = serde_json::from_str(&w.ok(&["status", "balerix", "--json"])).unwrap();
    assert_eq!(rec.status.agents["balerix/plugins/hello"].phase, AgentPhase::Ready);
    assert_eq!(rec.status.phase, FleetPhase::Ready);
    assert!(w.ok(&["list"]).contains("no fleets"), "plugins are not a fleet row");

    // the token is in the profile (0600) and nowhere else
    let profile = fs::read_to_string(plugin_dir.join("nono-profile.json")).unwrap();
    let token = profile
        .split("\"BALERIX_PLUGIN_TOKEN\": \"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();
    assert_eq!(token.len(), 64);
    assert!(!fs::read_to_string(plugin_dir.join("launch.sh")).unwrap().contains(&token));
    assert!(!fs::read_to_string(w.state().join("server/server.log")).unwrap().contains(&token));
    assert!(!pkg.join("escape").exists());

    // metrics
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let metrics = agent.get(format!("{url}/metrics")).call().unwrap().body_mut().read_to_string().unwrap();
    assert!(
        metrics.contains("balerix_agents{crew=\"plugins\",fleet=\"balerix\",phase=\"ready\"} 1"),
        "{metrics}"
    );

    // a user fleet may not take the reserved name
    let reserved = root.join("reserved.yaml");
    fs::write(&reserved, "apiVersion: balerix/v1\nkind: Fleet\nname: balerix\n").unwrap();
    let out = w.run(&["up", &reserved.display().to_string(), "--no-host-defaults", "--no-wait"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("reserved for the daemon's plugins"));

    // remove: stopped, window gone, state kept
    let out = w.ok(&["plugin", "remove", "hello"]);
    assert!(out.contains("stopped: hello"), "{out}");
    assert_eq!(w.ok(&["plugin", "list"]), "no plugins\n");
    let start = Instant::now();
    loop {
        let windows = Command::new(&w.tmux)
            .args(["-L", &w.socket, "list-windows", "-t", "=balerix/plugins", "-F", "#{window_name}"])
            .output()
            .unwrap();
        if !String::from_utf8_lossy(&windows.stdout).contains("hello") {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "plugin window still present");
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(plugin_dir.join("scratch").exists(), "state survives removal");

    // purge on an undeclared plugin deletes the state
    let out = w.ok(&["plugin", "remove", "hello", "--purge"]);
    assert!(out.contains("purged"), "{out}");
    assert!(!plugin_dir.exists(), "purge deletes plugins/hello");
    drop(w);
}
```

`plugin remove hello --purge` on an entry that is already gone must not fail: in `remove_command` (Task 7) treat "not declared" as fine when `--purge` is set — change the `bail!` to `if !remove_entry(..) && !args.purge { bail!(…) }` and add to the Task 7 unit test module a case `remove_entry` returning false with `--purge` still proceeding (covered here by the e2e). Update the `cli_plugin.rs` expectation: the offline `--purge` case fails on the daemon, not on the entry.

- [ ] **Step 2: Run the e2e**

Run: `mise run e2e`
Expected: both journeys PASS in under a minute. Failure diagnosis, in order: `plugin never became ready` with the nono log showing a denied path → a missing grant in `plugin_grants`; `mise run` complaining about an untrusted config → `MISE_STATE_DIR` differs between `install_plugin_tools` and `plugin_env`; `hello` 401 in the daemon log → the token in the profile is not the one in the index (the actor must have re-minted after materialize: check that `PluginMaterializer::materialize` receives the `HookTarget` from the executor unchanged); the `bind-failed` assertion → spec §11.1 row 1 verdict is FAILED, record it in Task 9 and stop; the Unix-socket fallback is phase 2's decision.

- [ ] **Step 3: Full check and commit**

Run: `mise run check`
Expected: PASS within the five-minute budget.

```bash
git add crates/balerix/tests/e2e.rs crates/balerix/src/commands/plugin.rs crates/balerix/tests/cli_plugin.rs
git commit -m "$(cat <<'EOF'
Add the plugin e2e: hello under nono, reserved fleet, remove and purge

Plugins spec §13 item 1 is done when a trivial SDK plugin reaches Ready
through a real `mise run` under nono. The journey declares a directory
package in plugins.yaml, starts the daemon, waits for `plugin list` to
show ready with a loopback listen address (spec §11.1 row 1: a sandboxed
plugin can bind), checks the token lives only in the 0600 profile, that
`balerix` is rejected as a fleet name, and that remove stops the window
while --purge deletes the state.

Claude-Session: https://claude.ai/code/session_01AMxZmNaRYWjQoNLWBsL6m6
EOF
)"
```

---

### Task 9: docs, threat model, spec verdicts and refinements

**Files:**
- Modify: `ARCHITECTURE.md`, `AGENTS.md`, `README.md`, `docs/THREAT-MODEL.md`, `docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md`, `docs/superpowers/specs/2026-09-05-balerix-architecture-design.md`

- [ ] **Step 1: `ARCHITECTURE.md`**

- "The pieces": add `balerix-plugin-sdk — the plugin side of the host protocol; depends on api only. Phase 1 ships Env and hello.` and extend the `balerix-server` bullet with `plugins/: plugins.yaml sync, package install, PluginHost`.
- "How it flows": add

  **Plugins (Spec B, phase 1):** `serve` syncs `$XDG_CONFIG_HOME/balerix/plugins.yaml` — installs tarballs under `$XDG_DATA_HOME/balerix/plugins/<name>/<digest12>/`, validates manifests — and renders the list as a synthetic fleet `balerix`/`plugins`/`<name>` that an ordinary fleet actor reconciles. `PluginMaterializer` maps each synthetic agent back to its plugin and the runtime materializes it like an agent under `plugins/<name>/` (`home/`, profile, `launch.sh` running `nono run → mise run <start>` from the package root). The actor's per-agent hook secret is the plugin's token; `POST /v1/plugin-host/hello` verifies it and is the plugin's `SessionStart`.
- "Non-obvious decisions": add
  - **Plugins ride the reconciler as a synthetic fleet.** `plugin_fleet()` renders plugins as agents whose only setting is the plugin hash; `plan`/`execute`/`apply` were not touched, and `cargo mutants` still covers them for plugins too (PB-5).
  - **The plugin token is the hook secret.** One minting path, one index, one constant-time check; `hello` is authenticated exactly like a hook event.
  - **`balerix` is a reserved fleet name.** Rejected client-side (`config resolve`) and by `POST`/`DELETE /v1/fleets`; readable through `GET /v1/fleets/balerix` and `balerix status balerix`; never a fleet-list row.
  - **`plugins.yaml` is the record.** The plugin actor persists nothing (`NullStore`); state survives removal under `plugins/<name>/` until `--purge`.
  - **The package's `mise.toml` is the global config.** One file, one `mise trust` covering both the daemon-side install and the sandboxed `mise run`.

- [ ] **Step 2: `AGENTS.md`**

- Conventions: add `balerix-plugin-sdk` to the ports/adapters paragraph: "`balerix-plugin-sdk` depends on `balerix-api` only."
- Gotchas: add
  - "`balerix` is a reserved fleet name (the plugin fleet). `FleetName` still parses it — the reservation lives in `Daemon::apply`/`down` and `balerix_config::resolve`."
  - "A plugin's token is the hook secret the fleet actor mints for `balerix/plugins/<name>`; it lives in `nono-profile.json` as `BALERIX_PLUGIN_TOKEN` and nowhere else."
  - "Plugin packages: `plugins.yaml` directory sources are used in place with no digest (development and the e2e); tarballs and URLs need `sha256` and unpack read-only under `$XDG_DATA_HOME/balerix/plugins/<name>/<digest12>/`."
  - "The daemon runs `mise trust` + `mise install` on the package's own `mise.toml` with `MISE_STATE_DIR` under the plugin's home; the sandbox uses the same dir, so if `mise run` says the config is untrusted, the two `MISE_STATE_DIR`s diverged (`plugin.rs`: `install_plugin_tools` vs `plugin_env`)."
  - "`balerix dev fake-plugin` is what the plugin e2e runs; it binds a loopback listener under nono and says hello through the SDK."
  - Extend the tmux-leftovers gotcha: the plugin e2e uses socket `balerix-e2e-plugins-<pid>`.

- [ ] **Step 3: `README.md`**

After quickstart step 6 add:

7. `mise x -- cargo run -q -p balerix -- plugin install ./my-plugin` — declares a plugin package (a directory with `mise.toml` and `balerix-plugin.yaml`) in `$XDG_CONFIG_HOME/balerix/plugins.yaml` and syncs the daemon; `plugin list` shows its phase, `plugin remove <name> [--purge]` takes it out. `plugin package <dir>` builds the tarball and prints the `sha256` a `plugins.yaml` entry needs.

Renumber the old step 7. Status paragraph: "Spec A is complete. Spec B (plugins) is in progress: phase 1, plugin workloads, is done — packages, `plugins.yaml`, the sandboxed plugin fleet and `hello`; the event protocol and the `flow` plugin are next."

- [ ] **Step 4: `docs/THREAT-MODEL.md`**

- Trust boundaries: add "**Plugin ↔ daemon** — operator-installed packages running sandboxed; `hello` and (later) the host protocol cross it over loopback HTTP with a per-launch token."
- Adversaries: add "**A compromised plugin** — runs inside its own nono profile with the daemon URL and its token. Wants: other plugins' or agents' state, the admin token."
- Accepted risks: add "**A plugin can read its package and the shared mise install dir** — both are read-only grants; a package is operator-installed, digest-checked content."
- Mitigations rows:
  - `Package unpacking` | digest verified before unpacking; absolute paths, `..`, symlinks, hard links and special entries rejected; files land 0444/0555 | `crates/balerix-server/src/plugins/package.rs`
  - `Plugin token` | 32 random bytes per launch, only in the 0600 `nono-profile.json` (`BALERIX_PLUGIN_TOKEN`) and the `Authorization` header; constant-time compare, unknown plugin and bad token answer alike; the SDK redacts it in `Debug` | `daemon.rs::plugin_hello`, `balerix-plugin-sdk/src/lib.rs`
  - `Plugin sandbox` | read: system dirs, shared mise dir, the package, the balerix and mise binaries; read-write: `home/` and `scratch/` only; `kv/` is never granted; `nono profile validate` before launch; manifest `sandbox` conflicts rejected | `crates/balerix-runtime/src/plugin.rs`
  - `Reserved fleet` | `balerix` refused for user fleets at the API and client-side, so no user spec can shadow the plugin fleet's ids or secrets | `daemon.rs::reject_reserved`, `balerix-config/src/resolve.rs`

- [ ] **Step 5: Spec verdicts and refinements**

In `docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md` §11.1, replace the first row's fallback cell with the verdict from Task 8: `**Verified 2026-09-06** (nono 0.75.0, e2e plugin_hello_journey): dev fake-plugin binds 127.0.0.1:0 inside the profile and reports it in hello; the Unix-socket fallback is not needed.` — or, if the `bind-failed` assertion fired, `**FAILED** …` with the error text and "phase 2 takes the Unix-socket fallback".

Append a section:

```markdown
## 15. Refinements from the phase 1 plan (2026-09-06)

- Plugins are driven through a synthetic `Fleet` (`balerix_core::plugin_fleet`); the reconciler is unchanged and `PluginMaterializer` maps the ids back.
- The plugin token is the actor's per-agent hook secret, delivered through the `HookTarget` the executor already passes; `hello` authenticates with `Daemon::verify_secret`.
- `MISE_GLOBAL_CONFIG_FILE` is the package's own `mise.toml`, not a copy.
- The tmux session is `balerix/plugins`.
- The reserved name is enforced at the API and in `balerix_config::resolve`, not in `FleetName`.
- `Materializer` gains `materialize_plugin` and `purge_plugin`.
- `PluginStatus` has no `active_agents` until Phase 2.
- `plugin install` syncs only when a daemon is running; `plugin remove --purge` requires one.
- Unpacked package directories are 0755 (files 0444/0555) so `--purge` is a plain `remove_dir_all`.
```

In `docs/superpowers/specs/2026-09-05-balerix-architecture-design.md` append:

```markdown
## Addendum 2026-09-06 (Spec B)
`docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md` §12 lists the
corrections Spec B made: §11 (Spec B is the plugin mechanism; `flow` is a
plugin), §3 (no `balerix-events`; `balerix-plugin-sdk` and in-tree plugin
crates), §5 (`plugins:` replaces `flow:`), §8 (`Outcome.actions` restored;
`balerix_plugin_flow_*` names), and the reserved fleet `balerix`. Where they
differ, the plugins spec wins.
```

- [ ] **Step 6: Commit**

Run: `mise run check` (docs only, but the pre-commit hook runs it anyway).

```bash
git add ARCHITECTURE.md AGENTS.md README.md docs/THREAT-MODEL.md docs/superpowers/specs
git commit -m "$(cat <<'EOF'
Document Spec B phase 1: plugin workloads, decisions, threat model, verdict

The map gains the plugin flow and four decisions (synthetic fleet, token is
the hook secret, reserved fleet, plugins.yaml is the record); the gotchas
cover the reservation, the token's one home, directory sources and the mise
trust state; the threat model gains the plugin boundary and its controls;
the plugins spec records the §11.1 bind verdict and the plan's refinements.

Claude-Session: https://claude.ai/code/session_01AMxZmNaRYWjQoNLWBsL6m6
EOF
)"
```

---

## Done when

- `mise run check` passes on a fresh clone here and in CI with `BALERIX_REQUIRE_TOOLS=1`, both e2e journeys included, inside the five-minute budget.
- By hand: a `plugins.yaml` with one directory source, `serve -d`, `plugin list` shows `ready` with a `127.0.0.1:<port>` listen address, `status balerix` shows the plugin agent `ready`, `plugin remove <name> --purge` empties `plugins/<name>/`.
- Spec §11.1 row 1 has a recorded verdict.
- `cargo mutants -p balerix-core` still reports no surviving mutants in `reconcile` (nothing there changed; run `mise run mutants` once to confirm).

## Deliberately deferred (phase 2 and 3 of the plugins spec)

- `activate`/`deactivate`, observers, the interceptor chain, `Outcome.actions`, the `fleets`/`actions`/`kv` host routes, metrics re-export, `docs/plugin-protocol.md` and the conformance test, `balerix-plugin-flow`.
- `/v1/plugins/<name>/*` reverse proxy with WebSocket passthrough, `AgentRunner::attach`, `balerix-plugin-web`, `mise run package-plugins`.
- `plugin install` for URL sources is implemented but untested end-to-end (no network in tests); the digest and unpack paths it uses are unit-tested.
- A plugin's `hooks` and `needs` are validated and stored but not acted on until phase 2.
