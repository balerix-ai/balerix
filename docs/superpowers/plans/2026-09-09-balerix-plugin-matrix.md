# Matrix Plugin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `balerix-plugin-matrix`, a plugin that posts curated agent events to a Matrix room per crew and a thread per agent session, and turns a reply in one of those threads into a `send_text` to that agent.

**Architecture:** One actor task owns every piece of mutable state and is fed by a bounded drop-oldest queue, so a thread root always exists before an event posts into it and a slow homeserver can never stall hook delivery. The actor is generic over a `MatrixPort` trait; the real implementation wraps `matrix-sdk`, and a fake makes every ordering rule testable without a homeserver. Two small changes to shared crates come first: `Plugin::configure` in the SDK, because `serve` discards the daemon-level config today, and a `secrets` map on `PluginEntry`, because a file-backed password cannot live in a directory that does not exist until the plugin starts.

**Tech Stack:** Rust 2024, `matrix-sdk 0.18.0`, `balerix-plugin-sdk`, `axum`, `tokio`, `prometheus`, `serde` with `serde_path_to_error`, `insta` snapshots, `cargo nextest`.

**Spec:** `docs/superpowers/specs/2026-09-09-balerix-g-matrix-plugin-design.md`

## Global Constraints

- Run cargo through mise: `mise x -- cargo …`, or a `mise run` task. Never bare `cargo`.
- `mise run check` (lint + test) must pass before every commit. The pre-commit hook runs it and gitleaks; redirect its output to a file, and make one commit per issue.
- The workspace forbids `unsafe`. `std::env::set_var` is unsafe in edition 2024, so inject environment through parameters.
- Clippy runs with `-D warnings` over `--all-targets`. `clippy.toml` sets `allow-unwrap-in-tests`, which covers `#[cfg(test)]` modules only. Files under `tests/` need `#![allow(clippy::unwrap_used, clippy::expect_used)]` at the top, as `crates/balerix-plugin-web/tests/plugin_it.rs:3` does.
- Every Cargo dependency is exact and lives in `[workspace.dependencies]` with a comment saying why. Member crates use `{ workspace = true }`.
- Plugin crates depend on `balerix-plugin-sdk` and `balerix-api` only, never on each other or on `balerix-server`.
- Library crates return `thiserror` errors whose message starts with the config path, as in `crews.backend.agents.bob.plugins.matrix.events[4]: …`. Only the binary uses `anyhow`.
- Types holding secrets hand-implement `Debug` and print `<redacted>`. Secrets never appear in argv, environment, logs or error messages.
- insta snapshots: read the `.snap.new`, compare it against the expected values written in the task, then `mise x -- cargo insta accept`. Never blind-accept.
- Config keys on the wire are camelCase (`userId`, `deviceId`), matching `apiVersion` in the plugin manifest.
- The plugin's outbound body limit is 4000 characters (`render::BODY_LIMIT`).
- The command queue depth is 1024, matching `balerix_api::OBSERVER_QUEUE`.

**Two refinements to the spec's §3 module list**, both for the same reason: keeping every testable thing out of the one file that cannot be unit-tested.

- `session.rs` is added, holding the sealed-KV session record and the pure restore-or-login decision. The spec put that in `matrix.rs`.
- `matrix.rs` keeps only the `MatrixPort` trait, its error and message types, and the in-memory fake. The `matrix-sdk` adapter moves to `client.rs`. The spec had both in `matrix.rs`.

Everything else follows §3 exactly.

## File Structure

**Shared crates, changed first:**

- `crates/balerix-plugin-sdk/src/plugin.rs` — add `Plugin::configure`, call it in `serve_on`.
- `crates/balerix-plugin-sdk/src/testing.rs` — `Harness::configure`, so plugins can test it.
- `crates/balerix-plugin-sdk/tests/conformance.rs` — the default and the failing case.
- `crates/balerix-api/src/plugin.rs` — `PluginEntry.secrets`.
- `crates/balerix-core/src/plugin.rs` — hand-written `Debug` for `ResolvedPlugin`, which now carries a resolved secret in `config`.
- `crates/balerix-server/src/plugins/config.rs` — `resolve_secrets`, the collision, missing-file and permission errors.
- `crates/balerix-server/src/plugins/host.rs:204` — `resolve` uses it instead of `entry.config.clone()`.

**The new crate:**

- `crates/balerix-plugin-matrix/src/config.rs` — `DaemonConfig`, `AgentConfig`, `Secret`, `ConfigError`. No I/O.
- `crates/balerix-plugin-matrix/src/render.rs` — hook event or phase change to markdown. Pure.
- `crates/balerix-plugin-matrix/src/matrix.rs` — the `MatrixPort` trait, `MatrixError`, `Inbound`, and the `matrix-sdk` adapter.
- `crates/balerix-plugin-matrix/src/session.rs` — the sealed session record and the restore-or-login decision.
- `crates/balerix-plugin-matrix/src/routing.rs` — room, thread and route maps, and their KV persistence.
- `crates/balerix-plugin-matrix/src/actor.rs` — `Command`, `Queue`, `Actor`. All mutable state.
- `crates/balerix-plugin-matrix/src/plugin.rs` — the SDK `Plugin` impl, the metric families, health.
- `crates/balerix-plugin-matrix/src/main.rs` — environment, tracing, wiring, `serve`.
- `crates/balerix-plugin-matrix/package/` — `balerix-plugin.yaml`, `mise.toml`.
- `crates/balerix-plugin-matrix/tests/plugin_it.rs` — the wire-format integration test.

**Docs:**

- `docs/THREAT-MODEL.md` — the accepted risk from spec §11.
- `AGENTS.md` — the gotchas this work produces.
- `scripts/verify-matrix.sh` — the documented manual check.

---
### Task 1: `Plugin::configure` in the SDK

The daemon-level config arrives in the `hello` reply and `serve` throws it away, so no plugin can read a homeserver or a credential today. One trait method with a no-op default fixes that and leaves flow and web untouched.

**Files:**
- Modify: `crates/balerix-plugin-sdk/src/lib.rs` (the `SdkError` enum)
- Modify: `crates/balerix-plugin-sdk/src/plugin.rs` (the `Plugin` trait, `serve_on`, its tests)
- Modify: `crates/balerix-plugin-sdk/src/testing.rs` (`Harness::spawn`)

**Interfaces:**
- Consumes: nothing.
- Produces: `Plugin::configure(&self, config: Value) -> impl Future<Output = Result<(), String>> + Send`, default `Ok(())`. `SdkError::Configure(String)`, displayed as `configure: {0}`. `Harness::start` now calls `configure` with the `FakeHost`'s config and panics on `Err`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module at the bottom of `crates/balerix-plugin-sdk/src/plugin.rs`:

```rust
    /// `serve` hands the hello reply's config to `configure`, and a
    /// rejection stops the server instead of leaving it listening.
    #[tokio::test]
    async fn serve_hands_the_hello_config_to_configure_and_a_rejection_stops_it() {
        use crate::testing::FakeHost;
        use std::sync::{Arc, Mutex};

        struct Recorder {
            seen: Arc<Mutex<Vec<Value>>>,
            reject: Option<String>,
        }
        impl Plugin for Recorder {
            async fn configure(&self, config: Value) -> Result<(), String> {
                self.seen
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(config);
                match &self.reject {
                    Some(m) => Err(m.clone()),
                    None => Ok(()),
                }
            }
        }

        let fake = FakeHost::start("tok", json!({ "homeserver": "https://h" }), Vec::new()).await;
        let env = fake.env("matrix", std::path::Path::new("scratch"));
        let host = Host::new(env).unwrap();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let err = serve(
            &host,
            "test",
            Recorder {
                seen: seen.clone(),
                reject: Some("bad homeserver".into()),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "configure: bad homeserver");
        let seen = seen.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(seen.len(), 1, "configure called once");
        assert_eq!(seen[0]["homeserver"], "https://h");
    }

    /// A plugin that implements nothing accepts any config.
    #[tokio::test]
    async fn the_default_configure_accepts_anything() {
        assert_eq!(Silent.configure(json!({ "anything": 1 })).await, Ok(()));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-plugin-sdk -E 'test(configure)'`
Expected: FAIL to compile, `no method named 'configure' found` and `no variant or associated item named 'Configure' found for enum 'SdkError'`.

- [ ] **Step 3: Add the error variant, the trait method and the `serve_on` call**

In `crates/balerix-plugin-sdk/src/lib.rs`, add a variant to `SdkError`:

```rust
    #[error("configure: {0}")]
    Configure(String),
```

In `crates/balerix-plugin-sdk/src/plugin.rs`, add to the `Plugin` trait, directly after `deactivate`:

```rust
    /// The daemon-level config from the `hello` reply (plugins spec §2.1).
    /// `serve` calls this once, after `hello` succeeds. `Err(message)`
    /// aborts the server and returns `SdkError::Configure`, so the process
    /// exits 1 and the daemon reports the plugin as not ready. The daemon
    /// may deliver an `activate` before this returns, so a plugin that
    /// needs the config must buffer until it arrives.
    fn configure(&self, config: Value) -> impl Future<Output = Result<(), String>> + Send {
        let _ = config;
        async { Ok(()) }
    }
```

In `serve_on`, replace the `hello` block with one that keeps the reply and calls `configure`:

```rust
    let plugin = Arc::new(plugin);
    let token = host.env().token.clone();
    let listener_plugin = plugin.clone();
    let server = tokio::spawn(async move { run(listener, listener_plugin, &token).await });
    let stop = |server: tokio::task::JoinHandle<Result<(), SdkError>>| async move {
        server.abort();
        let _ = server.await;
    };
    let reply = match host.hello(version, &listen).await {
        Ok(reply) => reply,
        Err(e) => {
            stop(server).await;
            return Err(e);
        }
    };
    if let Err(message) = plugin.configure(reply.config).await {
        stop(server).await;
        return Err(SdkError::Configure(message));
    }
    server.await.map_err(|e| SdkError::Bind(e.to_string()))?
```

In `crates/balerix-plugin-sdk/src/testing.rs`, make `Harness::spawn` do the same. Change its body so the `Arc` is created before `hello` and `configure` runs after it:

```rust
    async fn spawn<P: Plugin>(
        host: &Host,
        plugin: P,
        token: &str,
    ) -> (String, tokio::task::JoinHandle<Result<(), SdkError>>) {
        let (listener, listen) = bind().await.unwrap_or_else(|e| panic!("Harness bind: {e}"));
        let plugin = Arc::new(plugin);
        let served = plugin.clone();
        let token = token.to_string();
        let server = tokio::spawn(async move { run(listener, served, &token).await });
        let reply = host
            .hello("test", &listen)
            .await
            .unwrap_or_else(|e| panic!("Harness hello: {e}"));
        plugin
            .configure(reply.config)
            .await
            .unwrap_or_else(|e| panic!("Harness configure: {e}"));
        (listen, server)
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-plugin-sdk`
Expected: PASS, including the existing conformance tests. `docs/plugin-protocol/` gains no fixture, because `configure` is not a wire message; the fixture count assertion in `tests/conformance.rs` stays at 22.

- [ ] **Step 5: Run the full gate and commit**

Run: `mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log`
Expected: lint clean, all tests pass.

```bash
git add crates/balerix-plugin-sdk
git commit -m "Hand a plugin the daemon-level config from its hello reply"
```

---

### Task 2: `PluginEntry.secrets` in `balerix-api`

A file-backed password cannot live in the plugin's scratch directory, because scratch does not exist until the plugin is materialized, which is also when it first needs the password. The daemon reads the file instead. This task is only the wire type.

**Files:**
- Modify: `crates/balerix-api/src/plugin.rs` (`PluginEntry`, its tests)

**Interfaces:**
- Consumes: nothing.
- Produces: `PluginEntry.secrets: BTreeMap<String, PathBuf>`, defaulting empty and skipped when empty.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/balerix-api/src/plugin.rs`:

```rust
    #[test]
    fn an_entry_carries_secret_file_paths_and_omits_an_empty_map() {
        let f: PluginsFile = serde_json::from_value(json!({
            "plugins": [
                { "name": "matrix", "source": "./matrix",
                  "secrets": { "password": "../secrets/matrix-password" } },
                { "name": "web", "source": "./web" }
            ]
        }))
        .unwrap();
        assert_eq!(
            f.plugins[0].secrets.get("password").map(|p| p.as_path()),
            Some(std::path::Path::new("../secrets/matrix-password"))
        );
        assert!(f.plugins[1].secrets.is_empty(), "absent means empty");
        let back = serde_json::to_value(&f).unwrap();
        assert!(
            back["plugins"][1].get("secrets").is_none(),
            "an empty map is not serialized"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `mise x -- cargo nextest run -p balerix-api -E 'test(secret_file_paths)'`
Expected: FAIL, `unknown field 'secrets'`, because `PluginEntry` has `deny_unknown_fields`.

- [ ] **Step 3: Add the field**

In `crates/balerix-api/src/plugin.rs`, add `use std::path::PathBuf;` and `use std::collections::BTreeMap;` to the imports, then add to `PluginEntry` after `sha256`:

```rust
    /// Config keys whose values the daemon reads from a host file at load,
    /// so a secret need not be written into `plugins.yaml` (plugins spec
    /// G-7). Paths are relative to this file, like `source`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, PathBuf>,
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `mise x -- cargo nextest run -p balerix-api`
Expected: PASS. `plugins_file_round_trips_and_keeps_sources_verbatim` still passes because the field defaults.

- [ ] **Step 5: Run the full gate and commit**

Run: `mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log`

```bash
git add crates/balerix-api
git commit -m "Let a plugins.yaml entry name config keys backed by host files"
```

---

### Task 3: Resolve `secrets` in the daemon, and stop `Debug` printing them

The resolved config now carries a plaintext password, so `ResolvedPlugin` must stop deriving `Debug`. Both halves go in one commit because separating them would land a type that leaks in logs.

**Files:**
- Modify: `crates/balerix-core/src/plugin.rs` (`ResolvedPlugin` derives, a new `Debug` impl, its tests)
- Modify: `crates/balerix-server/src/plugins/config.rs` (`resolve_secrets`, its tests)
- Modify: `crates/balerix-server/src/plugins/host.rs:204` (use it in `resolve`)

**Interfaces:**
- Consumes: `PluginEntry.secrets` from Task 2.
- Produces: `balerix_server::plugins::config::resolve_secrets(entry: &PluginEntry, base: &Path) -> Result<Value, PluginError>`, returning the entry's `config` with each named key replaced by the file's contents. `ResolvedPlugin`'s `Debug` prints `config: <redacted>`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/balerix-server/src/plugins/config.rs`:

```rust
    fn secret_file(dir: &std::path::Path, name: &str, body: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    fn entry(config: Value, secrets: &[(&str, &str)]) -> PluginEntry {
        PluginEntry {
            name: "matrix".into(),
            source: "./matrix".into(),
            sha256: None,
            secrets: secrets
                .iter()
                .map(|(k, v)| (k.to_string(), PathBuf::from(v)))
                .collect(),
            config,
        }
    }

    #[test]
    fn a_secret_file_becomes_a_config_value_and_one_trailing_newline_is_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        secret_file(dir.path(), "pw", "hunter2\n", 0o600);
        let e = entry(json!({ "homeserver": "https://h" }), &[("password", "pw")]);
        let resolved = resolve_secrets(&e, dir.path()).unwrap();
        assert_eq!(resolved["password"], "hunter2");
        assert_eq!(resolved["homeserver"], "https://h");
    }

    #[test]
    fn a_secret_that_collides_with_a_config_key_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        secret_file(dir.path(), "pw", "hunter2", 0o600);
        let e = entry(json!({ "password": "inline" }), &[("password", "pw")]);
        let err = resolve_secrets(&e, dir.path()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "secrets.password: collides with config.password"
        );
    }

    #[test]
    fn a_missing_secret_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let e = entry(json!({}), &[("password", "nope")]);
        let err = resolve_secrets(&e, dir.path()).unwrap_err();
        assert!(
            err.to_string().starts_with("secrets.password: "),
            "path first: {err}"
        );
        assert!(err.to_string().contains("nope"), "names the file: {err}");
    }

    #[test]
    fn a_secret_file_readable_by_group_or_other_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        secret_file(dir.path(), "pw", "hunter2", 0o644);
        let e = entry(json!({}), &[("password", "pw")]);
        let err = resolve_secrets(&e, dir.path()).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "secrets.password: {} is mode 0644, expected no group or other access",
                dir.path().join("pw").display()
            )
        );
        secret_file(dir.path(), "pw", "hunter2", 0o400);
        assert!(
            resolve_secrets(&e, dir.path()).is_ok(),
            "0400 is as acceptable as 0600"
        );
    }

    #[test]
    fn a_non_object_config_with_secrets_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        secret_file(dir.path(), "pw", "hunter2", 0o600);
        let e = entry(json!([]), &[("password", "pw")]);
        let err = resolve_secrets(&e, dir.path()).unwrap_err();
        assert_eq!(err.to_string(), "secrets: config is not a map");
    }
```

Add to the `tests` module in `crates/balerix-core/src/plugin.rs`:

```rust
    #[test]
    fn debug_never_prints_the_resolved_config() {
        let mut p = plugin_fixture();
        p.config = serde_json::json!({ "password": "hunter2" });
        let text = format!("{p:?}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
        assert!(text.contains("name"), "the rest is still readable: {text}");
    }
```

If the file has no `plugin_fixture` helper, add one that builds a `ResolvedPlugin` from the existing test manifest fixture in that module.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-server -E 'test(secret)'` and `mise x -- cargo nextest run -p balerix-core -E 'test(debug_never_prints)'`
Expected: FAIL to compile, `cannot find function 'resolve_secrets'`, and the core test failing on `hunter2` appearing in the derived `Debug`.

- [ ] **Step 3: Write the implementation**

In `crates/balerix-server/src/plugins/config.rs`, add `use std::collections::BTreeMap;`, `use balerix_api::PluginEntry;` (already imported), `use serde_json::Value;` and:

```rust
/// The entry's `config` with every `secrets` key replaced by the contents
/// of its file (plugins spec G-7). Paths resolve against the directory
/// holding `plugins.yaml`, like `source`. Errors name the `secrets.<key>`
/// path and never the value.
pub fn resolve_secrets(entry: &PluginEntry, base: &Path) -> Result<Value, PluginError> {
    use std::os::unix::fs::PermissionsExt;

    if entry.secrets.is_empty() {
        return Ok(entry.config.clone());
    }
    let err = |key: &str, message: String| PluginError::Config {
        path: format!("secrets.{key}"),
        message,
    };
    let mut config = entry.config.clone();
    let Some(map) = config.as_object_mut() else {
        return Err(PluginError::Config {
            path: "secrets".into(),
            message: "config is not a map".into(),
        });
    };
    for (key, rel) in &entry.secrets {
        if map.contains_key(key) {
            return Err(err(key, format!("collides with config.{key}")));
        }
        let path = if rel.is_absolute() {
            rel.clone()
        } else {
            base.join(rel)
        };
        let meta = std::fs::metadata(&path)
            .map_err(|e| err(key, format!("{}: {e}", path.display())))?;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(err(
                key,
                format!(
                    "{} is mode {mode:04o}, expected no group or other access",
                    path.display()
                ),
            ));
        }
        let body = std::fs::read_to_string(&path)
            .map_err(|e| err(key, format!("{}: {e}", path.display())))?;
        let body = body.strip_suffix('\n').unwrap_or(&body).to_string();
        map.insert(key.clone(), Value::String(body));
    }
    Ok(config)
}
```

In `crates/balerix-server/src/plugins/host.rs`, in `resolve`, replace `config: entry.config.clone(),` with:

```rust
                config: resolve_secrets(entry, &base)
                    .map_err(|e| entry_error(i, "", e.to_string()))?,
```

and add `resolve_secrets` to the `use super::config::{…}` list at the top of the file.

In `crates/balerix-core/src/plugin.rs`, drop `Debug` from `ResolvedPlugin`'s derive list, leaving `#[derive(Clone, PartialEq)]`, and add:

```rust
/// `config` may carry a resolved secret (plugins spec G-7), so it is never
/// printed. Everything else is, because it is what a log line is for.
impl std::fmt::Debug for ResolvedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedPlugin")
            .field("name", &self.name)
            .field("package", &self.package)
            .field("manifest", &self.manifest)
            .field("config", &"<redacted>")
            .field("digest", &self.digest)
            .finish()
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-core -p balerix-server`
Expected: PASS. If a `balerix-server` test asserted on `ResolvedPlugin`'s derived `Debug` output, update it to the new shape rather than weakening the redaction.

- [ ] **Step 5: Run the full gate and commit**

Run: `mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log`

```bash
git add crates/balerix-api crates/balerix-core crates/balerix-server
git commit -m "Read a plugin's file-backed secrets in the daemon, and never print them"
```

---
### Task 4: The crate skeleton and `config.rs`

The two config blocks of spec §4, with no I/O. The daemon has already read any file-backed secret, so this module only parses. No external dependency is added here; `matrix-sdk` arrives in Task 12, so Tasks 4 through 11 build and test with nothing new in the lock file.

**Files:**
- Create: `crates/balerix-plugin-matrix/Cargo.toml`
- Create: `crates/balerix-plugin-matrix/src/lib.rs`
- Create: `crates/balerix-plugin-matrix/src/config.rs`
- Modify: `Cargo.toml` (workspace `[workspace.dependencies]`)

**Interfaces:**
- Consumes: `balerix_api::HOOK_EVENTS`.
- Produces: `Secret` with `new`, `expose` and a redacting `Debug`. `DaemonConfig { homeserver, user_id, password: Option<Secret>, device_id, device_name, invite: Vec<String>, rooms: BTreeMap<String, String> }`. `AgentConfig { enabled: bool, events: Vec<String>, phases: bool }` with `wants(&self, event: &str) -> bool`. `ConfigError { path, message }`. `parse_daemon(&Value) -> Result<DaemonConfig, ConfigError>` and `parse_agent(&Value) -> Result<AgentConfig, ConfigError>`. `DEFAULT_EVENTS` and `LIFECYCLE`.

- [ ] **Step 1: Create the crate and register it**

`crates/balerix-plugin-matrix/Cargo.toml`:

```toml
[package]
name = "balerix-plugin-matrix"
description = "The matrix plugin: agent events to a Matrix room per crew, a thread per agent session, and thread replies back as send_text (Spec G)"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
publish.workspace = true

[[bin]]
name = "balerix-plugin-matrix"
path = "src/main.rs"

[dependencies]
balerix-api = { workspace = true }
balerix-plugin-sdk = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
serde_path_to_error = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
anyhow = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }

[dev-dependencies]
insta = { workspace = true }
tempfile = { workspace = true }

[lints]
workspace = true
```

`src/lib.rs`:

```rust
//! The matrix plugin (Spec G): a room per crew, a thread per agent
//! session, and a thread reply back to that agent as `send_text`.

pub mod config;
```

`src/main.rs`, a placeholder replaced in Task 12 so the `[[bin]]` target compiles:

```rust
fn main() {
    eprintln!("matrix: not wired yet");
    std::process::exit(1);
}
```

In the workspace `Cargo.toml`, add to `[workspace.dependencies]` beside the other plugin crates:

```toml
balerix-plugin-matrix = { path = "crates/balerix-plugin-matrix" }
```

- [ ] **Step 2: Write the failing tests**

Create `crates/balerix-plugin-matrix/src/config.rs` containing only the `tests` module below plus `use` lines, so the test names exist before the implementation:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn daemon() -> Value {
        json!({
            "homeserver": "https://matrix.example.org",
            "userId": "@balerix:example.org",
            "password": "hunter2"
        })
    }

    #[test]
    fn daemon_config_defaults_the_device_and_keeps_the_secret_out_of_debug() {
        let c = parse_daemon(&daemon()).unwrap();
        assert_eq!(c.homeserver, "https://matrix.example.org");
        assert_eq!(c.user_id, "@balerix:example.org");
        assert_eq!(c.device_id, "balerix");
        assert_eq!(c.device_name, "balerix");
        assert!(c.invite.is_empty() && c.rooms.is_empty());
        assert_eq!(c.password.as_ref().map(Secret::expose), Some("hunter2"));
        let text = format!("{c:?}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
    }

    #[test]
    fn daemon_config_reads_camel_case_and_rejects_unknown_keys() {
        let mut v = daemon();
        v["deviceId"] = json!("laptop");
        v["deviceName"] = json!("balerix on laptop");
        v["invite"] = json!(["@rahul:example.org"]);
        v["rooms"] = json!({ "payments/backend": "!abc:example.org" });
        let c = parse_daemon(&v).unwrap();
        assert_eq!(c.device_id, "laptop");
        assert_eq!(c.device_name, "balerix on laptop");
        assert_eq!(c.invite, vec!["@rahul:example.org".to_string()]);
        assert_eq!(
            c.rooms.get("payments/backend").map(String::as_str),
            Some("!abc:example.org")
        );

        let mut v = daemon();
        v["nope"] = json!(1);
        assert_eq!(
            parse_daemon(&v).unwrap_err().to_string(),
            "nope: unknown field `nope`"
        );
    }

    #[test]
    fn daemon_config_rejects_a_bad_homeserver_or_user_id() {
        let mut v = daemon();
        v["homeserver"] = json!("matrix.example.org");
        assert_eq!(
            parse_daemon(&v).unwrap_err().to_string(),
            "homeserver: must start with https:// or http://"
        );
        let mut v = daemon();
        v["userId"] = json!("balerix:example.org");
        assert_eq!(
            parse_daemon(&v).unwrap_err().to_string(),
            "userId: must be a full Matrix id starting with @"
        );
        let v = json!({ "userId": "@a:b" });
        assert_eq!(
            parse_daemon(&v).unwrap_err().to_string(),
            "homeserver: missing field `homeserver`"
        );
    }

    #[test]
    fn agent_config_defaults_to_the_curated_set_and_always_wants_lifecycle() {
        let c = parse_agent(&json!({})).unwrap();
        assert!(c.enabled);
        assert!(c.phases);
        assert_eq!(c.events, DEFAULT_EVENTS.map(String::from).to_vec());
        assert!(c.wants("Notification"));
        assert!(!c.wants("PreToolUse"));

        let c = parse_agent(&json!({ "events": ["PreToolUse"], "phases": false })).unwrap();
        assert!(!c.phases);
        assert!(c.wants("PreToolUse"));
        assert!(!c.wants("Notification"), "the list replaces the default");
        assert!(
            c.wants("SessionStart") && c.wants("SessionEnd"),
            "lifecycle is always posted"
        );
    }

    #[test]
    fn agent_config_rejects_unknown_keys_and_unknown_events_with_their_index() {
        assert_eq!(
            parse_agent(&json!({ "enable": true })).unwrap_err().to_string(),
            "enable: unknown field `enable`"
        );
        assert_eq!(
            parse_agent(&json!({ "events": ["Stop", "Frobnicate"] }))
                .unwrap_err()
                .to_string(),
            "events[1]: unknown event \"Frobnicate\""
        );
        assert!(parse_agent(&json!([])).unwrap_err().path.is_empty());
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix`
Expected: FAIL to compile, `cannot find function 'parse_daemon'`.

- [ ] **Step 4: Write the implementation**

Prepend to `crates/balerix-plugin-matrix/src/config.rs`:

```rust
//! The two config blocks (Spec G §4). `DaemonConfig` arrives in the hello
//! reply, `AgentConfig` at `activate`. No I/O: the daemon has already read
//! any file-backed secret (§5.1), so a password is a plain value here.

use std::collections::BTreeMap;
use std::fmt;

use balerix_api::HOOK_EVENTS;
use serde::Deserialize;
use serde_json::Value;

/// The curated default event set (Spec G §4.2).
pub const DEFAULT_EVENTS: [&str; 4] = ["SessionStart", "Notification", "Stop", "SessionEnd"];
/// Always posted: these open and close the thread, so `events` cannot
/// suppress them.
pub const LIFECYCLE: [&str; 2] = ["SessionStart", "SessionEnd"];

/// A credential. Hand-written `Debug` printing `<redacted>`, per AGENTS.md.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DaemonConfig {
    pub homeserver: String,
    pub user_id: String,
    #[serde(default)]
    pub password: Option<Secret>,
    #[serde(default = "default_device")]
    pub device_id: String,
    #[serde(default = "default_device")]
    pub device_name: String,
    #[serde(default)]
    pub invite: Vec<String>,
    /// `fleet/crew` to room id (Spec G §7).
    #[serde(default)]
    pub rooms: BTreeMap<String, String>,
}

fn default_device() -> String {
    "balerix".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentConfig {
    pub enabled: bool,
    pub events: Vec<String>,
    pub phases: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            events: DEFAULT_EVENTS.iter().map(|e| (*e).to_string()).collect(),
            phases: true,
        }
    }
}

impl AgentConfig {
    /// Lifecycle events are always posted; everything else is filtered by
    /// `events` (Spec G §4.2).
    pub fn wants(&self, event: &str) -> bool {
        LIFECYCLE.contains(&event) || self.events.iter().any(|e| e == event)
    }
}

/// One line, config path first; an empty path prints the message alone.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct ConfigError {
    pub path: String,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

/// Serde's message trimmed to its first clause, with its path attached.
/// A non-map is rejected first: both structs are fully defaulted or have a
/// `default` on the struct, so serde would otherwise read a bare array as a
/// sequence of zero fields, exactly as `balerix-plugin-web` documents.
fn deserialize<T: serde::de::DeserializeOwned>(config: &Value) -> Result<T, ConfigError> {
    if !config.is_object() {
        let kind = match config {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        return Err(ConfigError {
            path: String::new(),
            message: format!("invalid type: {kind}, expected a map"),
        });
    }
    serde_path_to_error::deserialize(config.clone()).map_err(|e| {
        let path = match e.path().to_string() {
            p if p == "." => String::new(),
            p => p,
        };
        let inner = e.into_inner().to_string();
        let message = inner
            .split(", expected one of")
            .next()
            .unwrap_or(&inner)
            .split(", expected `")
            .next()
            .unwrap_or(&inner)
            .to_string();
        ConfigError { path, message }
    })
}

pub fn parse_daemon(config: &Value) -> Result<DaemonConfig, ConfigError> {
    let c: DaemonConfig = deserialize(config)?;
    if !(c.homeserver.starts_with("https://") || c.homeserver.starts_with("http://")) {
        return Err(ConfigError {
            path: "homeserver".into(),
            message: "must start with https:// or http://".into(),
        });
    }
    if !c.user_id.starts_with('@') {
        return Err(ConfigError {
            path: "userId".into(),
            message: "must be a full Matrix id starting with @".into(),
        });
    }
    Ok(c)
}

pub fn parse_agent(config: &Value) -> Result<AgentConfig, ConfigError> {
    let c: AgentConfig = deserialize(config)?;
    for (i, name) in c.events.iter().enumerate() {
        if !HOOK_EVENTS.contains(&name.as_str()) {
            return Err(ConfigError {
                path: format!("events[{i}]"),
                message: format!("unknown event {name:?}"),
            });
        }
    }
    Ok(c)
}
```

Note on the missing-field path: `serde_path_to_error` reports a missing field with the field name as its path, which is what `daemon_config_rejects_a_bad_homeserver_or_user_id` asserts. If the observed message differs, fix the test to the observed string rather than the other way round, and keep the path-first shape.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix`
Expected: PASS, five tests.

- [ ] **Step 6: Run the full gate and commit**

Run: `mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log`

```bash
git add Cargo.toml Cargo.lock crates/balerix-plugin-matrix
git commit -m "Add the matrix plugin crate and its two config blocks"
```

---

### Task 5: `render.rs`

Every message body the plugin ever sends, as pure functions over a hook event or a phase change. Pure means every case is a snapshot test and none of them need a homeserver.

**Files:**
- Create: `crates/balerix-plugin-matrix/src/render.rs`
- Modify: `crates/balerix-plugin-matrix/src/lib.rs`

**Interfaces:**
- Consumes: `balerix_api::{AgentPhase, HookEvent}`.
- Produces: `BODY_LIMIT: usize = 4000`. `PhaseChange { agent: String, from: AgentPhase, to: AgentPhase, message: String }`. `thread_root(agent: &str, session_id: &str, source: &str) -> String`. `event_message(&HookEvent) -> String`. `phase_message(&PhaseChange) -> String`. `truncate(&str) -> String`. `short_session(&str) -> &str`.

- [ ] **Step 1: Write the failing tests**

Create `crates/balerix-plugin-matrix/src/render.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use balerix_plugin_sdk::testing::event;
    use serde_json::json;

    #[test]
    fn a_thread_root_names_the_agent_the_session_and_the_source() {
        insta::assert_snapshot!(thread_root(
            "payments/backend/alice",
            "0199aa11-2233-4455-6677-889900aabbcc",
            "startup"
        ));
    }

    #[test]
    fn every_event_renders() {
        let cases = [
            ("SessionStart", json!({ "source": "compact" })),
            ("SessionEnd", json!({ "reason": "clear" })),
            (
                "Notification",
                json!({ "message": "Claude needs your permission to use Bash" }),
            ),
            ("Stop", json!({ "stop_hook_active": false })),
            ("SubagentStop", json!({})),
            ("UserPromptSubmit", json!({ "prompt": "run the tests" })),
            (
                "PreToolUse",
                json!({ "tool_name": "Bash", "tool_input": { "command": "cargo test" } }),
            ),
            (
                "PostToolUse",
                json!({ "tool_name": "Write", "tool_input": { "file_path": "src/a.rs" } }),
            ),
            ("PreCompact", json!({ "trigger": "auto" })),
        ];
        let rendered: Vec<String> = cases
            .iter()
            .map(|(name, payload)| {
                format!(
                    "{name}\n{}",
                    event_message(&event("f/c/a", name, payload.clone()))
                )
            })
            .collect();
        insta::assert_snapshot!(rendered.join("\n\n---\n\n"));
    }

    #[test]
    fn a_missing_payload_field_still_renders() {
        let e = event("f/c/a", "Notification", json!({}));
        assert!(!event_message(&e).is_empty());
        let e = event("f/c/a", "PreToolUse", json!({}));
        assert!(!event_message(&e).is_empty());
    }

    #[test]
    fn a_phase_change_names_both_phases_and_the_message() {
        insta::assert_snapshot!(phase_message(&PhaseChange {
            agent: "payments/backend/alice".into(),
            from: AgentPhase::Ready,
            to: AgentPhase::Failed,
            message: "tmux window gone".into(),
        }));
    }

    #[test]
    fn truncate_cuts_at_a_line_boundary_and_says_so() {
        let short = "one\ntwo";
        assert_eq!(truncate(short), short, "under the limit is untouched");
        let long = "abcd\n".repeat(2000);
        let cut = truncate(&long);
        assert!(cut.len() <= BODY_LIMIT + 32, "len {}", cut.len());
        assert!(cut.ends_with("truncated"), "{}", &cut[cut.len() - 40..]);
        assert!(
            cut.trim_end_matches("\n\n… truncated").ends_with("abcd"),
            "cut on a line boundary"
        );
    }

    #[test]
    fn a_short_session_is_the_first_eight_characters() {
        assert_eq!(short_session("0199aa11-2233-4455"), "0199aa11");
        assert_eq!(short_session("abc"), "abc");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix -E 'test(render)'`
Expected: FAIL to compile, `cannot find function 'thread_root'`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/balerix-plugin-matrix/src/render.rs`:

```rust
//! Every message body the plugin sends (Spec G §8), as pure functions.
//! Output is markdown; the adapter turns it into a plain `body` and an
//! HTML `formatted_body`.

use balerix_api::{AgentPhase, HookEvent};
use serde_json::Value;

/// Bodies are cut here (Spec G §8): comfortably under the 64 KiB event
/// limit a homeserver enforces, and the limit OpenClaw defaults to.
pub const BODY_LIMIT: usize = 4000;

/// One agent's phase transition, from the fleet watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseChange {
    pub agent: String,
    pub from: AgentPhase,
    pub to: AgentPhase,
    pub message: String,
}

/// The first eight characters of a session id: enough to tell two apart
/// in a room, short enough to read on a phone.
pub fn short_session(session_id: &str) -> &str {
    let end = session_id
        .char_indices()
        .nth(8)
        .map(|(i, _)| i)
        .unwrap_or(session_id.len());
    &session_id[..end]
}

/// The message a thread is rooted on.
pub fn thread_root(agent: &str, session_id: &str, source: &str) -> String {
    let name = agent.rsplit('/').next().unwrap_or(agent);
    format!(
        "**{name}** session `{}` started ({source})\n\n`{agent}`",
        short_session(session_id)
    )
}

fn text<'a>(payload: &'a Value, key: &str, fallback: &'a str) -> &'a str {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(fallback)
}

/// A tool call in one line: the field that says what it touched, when the
/// tool has an obvious one.
fn tool_summary(payload: &Value) -> String {
    let name = text(payload, "tool_name", "tool");
    let input = payload.get("tool_input");
    let field = |key: &str| {
        input
            .and_then(|i| i.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    match field("command").or_else(|| field("file_path")).or_else(|| field("pattern")) {
        Some(detail) => format!("`{name}` {detail}"),
        None => format!("`{name}`"),
    }
}

pub fn event_message(event: &HookEvent) -> String {
    let p = &event.payload;
    let body = match event.name.as_str() {
        "SessionStart" => format!("session restarted ({})", text(p, "source", "unknown")),
        "SessionEnd" => format!("**session ended** ({})", text(p, "reason", "unknown")),
        "Notification" => format!("**needs you:** {}", text(p, "message", "notification")),
        "Stop" => "**turn finished**".to_string(),
        "SubagentStop" => "subagent finished".to_string(),
        "UserPromptSubmit" => format!("**prompt**\n\n{}", text(p, "prompt", "(empty)")),
        "PreToolUse" => format!("running {}", tool_summary(p)),
        "PostToolUse" => format!("finished {}", tool_summary(p)),
        "PreCompact" => format!("compacting ({})", text(p, "trigger", "unknown")),
        other => other.to_string(),
    };
    truncate(&body)
}

pub fn phase_message(change: &PhaseChange) -> String {
    let base = format!("phase **{:?}** to **{:?}**", change.from, change.to);
    let body = if change.message.trim().is_empty() {
        base
    } else {
        format!("{base}: {}", change.message)
    };
    truncate(&body)
}

/// Cut at the last line boundary inside the limit, or at the limit when a
/// single line is longer than it.
pub fn truncate(text: &str) -> String {
    if text.len() <= BODY_LIMIT {
        return text.to_string();
    }
    let mut end = BODY_LIMIT;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let head = &text[..end];
    let cut = head.rfind('\n').map(|i| &head[..i]).unwrap_or(head);
    format!("{cut}\n\n… truncated")
}
```

Add `pub mod render;` to `src/lib.rs`.

- [ ] **Step 4: Run the tests and review the snapshots**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix`
Expected: FAIL, with four `.snap.new` files written under `crates/balerix-plugin-matrix/src/snapshots/`.

Read each `.snap.new` and check it against these expectations before accepting:
- the thread root names `alice`, the short session `0199aa11`, the source `startup`, and the full agent id;
- `Notification` renders the permission sentence verbatim after a `needs you:` label;
- `Stop` is a single short line with no payload noise;
- `PreToolUse` shows `Bash` and `cargo test`, `PostToolUse` shows `Write` and `src/a.rs`;
- the phase change names `Ready`, `Failed` and the message.

Then: `mise x -- cargo insta accept`

- [ ] **Step 5: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix`
Expected: PASS.

- [ ] **Step 6: Run the full gate and commit**

Run: `mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log`

```bash
git add crates/balerix-plugin-matrix
git commit -m "Render every matrix message body as a pure function"
```

---
### Task 6: The `MatrixPort` trait and its fake

The seam that makes the actor testable. The trait's methods return `impl Future`, so they are not dyn-compatible; the actor takes the port as a type parameter, per spec G-13. The fake is always compiled, mirroring `balerix_plugin_sdk::testing`, because the integration test in Task 12 needs it too.

**Files:**
- Create: `crates/balerix-plugin-matrix/src/matrix.rs`
- Modify: `crates/balerix-plugin-matrix/src/lib.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `MatrixError` with variants `RateLimited { retry_after_ms: u64 }`, `Auth(String)`, `Other(String)`. `Inbound { room, event_id, sender, thread_root: Option<String>, body }`. `MatrixPort` with `user_id(&self) -> &str`, `create_room(&self, name: &str, invite: &[String]) -> Result<String, MatrixError>`, `send(&self, room: &str, thread_root: Option<&str>, markdown: &str) -> Result<String, MatrixError>`, `react(&self, room: &str, event_id: &str, key: &str) -> Result<(), MatrixError>`, the last three async. Reaction keys `ACK`, `REFUSED`, `FAILED`. `matrix::fake::{FakePort, Call}`.

- [ ] **Step 1: Write the failing test**

Create `crates/balerix-plugin-matrix/src/matrix.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::fake::{Call, FakePort};
    use super::*;

    #[tokio::test]
    async fn the_fake_records_calls_mints_ids_and_can_be_made_to_fail() {
        let p = FakePort::new("@balerix:example.org");
        assert_eq!(p.user_id(), "@balerix:example.org");

        let room = p
            .create_room("balerix payments/backend", &["@rahul:example.org".into()])
            .await
            .unwrap();
        let root = p.send(&room, None, "root").await.unwrap();
        let child = p.send(&room, Some(&root), "child").await.unwrap();
        p.react(&room, &child, ACK).await.unwrap();
        assert_ne!(root, child, "every send mints a fresh event id");

        assert_eq!(
            p.calls(),
            vec![
                Call::CreateRoom {
                    name: "balerix payments/backend".into(),
                    invite: vec!["@rahul:example.org".into()],
                },
                Call::Send {
                    room: room.clone(),
                    thread_root: None,
                    body: "root".into(),
                },
                Call::Send {
                    room: room.clone(),
                    thread_root: Some(root.clone()),
                    body: "child".into(),
                },
                Call::React {
                    room,
                    event_id: child,
                    key: ACK.into(),
                },
            ]
        );

        p.fail_next(MatrixError::RateLimited { retry_after_ms: 250 });
        let err = p.send("!r:fake", None, "x").await.unwrap_err();
        assert_eq!(err.to_string(), "rate limited, retry in 250 ms");
        assert!(
            p.send("!r:fake", None, "x").await.is_ok(),
            "only the next call fails"
        );
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix -E 'test(the_fake_records_calls)'`
Expected: FAIL to compile, `cannot find module 'fake'`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/balerix-plugin-matrix/src/matrix.rs`:

```rust
//! The Matrix seam (Spec G §13). `MatrixPort` is what the actor is written
//! against; `fake::FakePort` is the in-memory implementation its tests use,
//! and `MatrixClient` (Task 12) is the `matrix-sdk` one. Inbound messages
//! are not on the trait: the adapter owns a task that pushes them into the
//! actor's queue, so the trait stays a plain request-response surface.

use std::future::Future;

/// The acknowledgement reactions (Spec G §9).
pub const ACK: &str = "👍";
/// A message the plugin declined to route.
pub const REFUSED: &str = "🚫";
/// A routed message the daemon rejected.
pub const FAILED: &str = "❗";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MatrixError {
    #[error("rate limited, retry in {retry_after_ms} ms")]
    RateLimited { retry_after_ms: u64 },
    #[error("auth: {0}")]
    Auth(String),
    #[error("{0}")]
    Other(String),
}

/// One message the plugin saw in a room it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    pub room: String,
    pub event_id: String,
    pub sender: String,
    /// The `m.thread` relation's root, when the message has one.
    pub thread_root: Option<String>,
    pub body: String,
}

pub trait MatrixPort: Send + Sync + 'static {
    /// The account the plugin is logged in as; used for loop protection.
    fn user_id(&self) -> &str;
    /// A private, encrypted room, with everyone in `invite` invited.
    /// Returns the room id.
    fn create_room(
        &self,
        name: &str,
        invite: &[String],
    ) -> impl Future<Output = Result<String, MatrixError>> + Send;
    /// Sends markdown, as a thread reply when `thread_root` is set.
    /// Returns the new event id.
    fn send(
        &self,
        room: &str,
        thread_root: Option<&str>,
        markdown: &str,
    ) -> impl Future<Output = Result<String, MatrixError>> + Send;
    fn react(
        &self,
        room: &str,
        event_id: &str,
        key: &str,
    ) -> impl Future<Output = Result<(), MatrixError>> + Send;
}

/// An in-memory `MatrixPort` for tests. Always compiled, like
/// `balerix_plugin_sdk::testing`, because the integration test needs it.
pub mod fake {
    use std::sync::{Arc, Mutex};

    use super::{MatrixError, MatrixPort};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Call {
        CreateRoom {
            name: String,
            invite: Vec<String>,
        },
        Send {
            room: String,
            thread_root: Option<String>,
            body: String,
        },
        React {
            room: String,
            event_id: String,
            key: String,
        },
    }

    #[derive(Default)]
    struct Inner {
        calls: Vec<Call>,
        next: u64,
        fail_next: Option<MatrixError>,
    }

    #[derive(Clone)]
    pub struct FakePort {
        user_id: String,
        inner: Arc<Mutex<Inner>>,
    }

    impl FakePort {
        pub fn new(user_id: &str) -> Self {
            Self {
                user_id: user_id.to_string(),
                inner: Arc::new(Mutex::new(Inner::default())),
            }
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
            self.inner.lock().unwrap_or_else(|e| e.into_inner())
        }

        pub fn calls(&self) -> Vec<Call> {
            self.lock().calls.clone()
        }

        /// Clears the recorded calls; handy between phases of a test.
        pub fn take_calls(&self) -> Vec<Call> {
            std::mem::take(&mut self.lock().calls)
        }

        /// The next call, whichever it is, fails with this error once.
        pub fn fail_next(&self, error: MatrixError) {
            self.lock().fail_next = Some(error);
        }

        fn check(&self) -> Result<(), MatrixError> {
            match self.lock().fail_next.take() {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }

        fn mint(&self, prefix: &str) -> String {
            let mut inner = self.lock();
            inner.next += 1;
            format!("{prefix}{}:fake", inner.next)
        }
    }

    impl MatrixPort for FakePort {
        fn user_id(&self) -> &str {
            &self.user_id
        }

        async fn create_room(
            &self,
            name: &str,
            invite: &[String],
        ) -> Result<String, MatrixError> {
            self.check()?;
            self.lock().calls.push(Call::CreateRoom {
                name: name.to_string(),
                invite: invite.to_vec(),
            });
            Ok(self.mint("!room"))
        }

        async fn send(
            &self,
            room: &str,
            thread_root: Option<&str>,
            markdown: &str,
        ) -> Result<String, MatrixError> {
            self.check()?;
            self.lock().calls.push(Call::Send {
                room: room.to_string(),
                thread_root: thread_root.map(str::to_string),
                body: markdown.to_string(),
            });
            Ok(self.mint("$evt"))
        }

        async fn react(
            &self,
            room: &str,
            event_id: &str,
            key: &str,
        ) -> Result<(), MatrixError> {
            self.check()?;
            self.lock().calls.push(Call::React {
                room: room.to_string(),
                event_id: event_id.to_string(),
                key: key.to_string(),
            });
            Ok(())
        }
    }
}
```

Add `pub mod matrix;` to `src/lib.rs`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix`
Expected: PASS.

- [ ] **Step 5: Run the full gate and commit**

Run: `mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log`

```bash
git add crates/balerix-plugin-matrix
git commit -m "Put a port between the matrix plugin and any Matrix client"
```

---

### Task 7: `routing.rs`

The room, thread and route maps and their sealed-KV persistence. `threads` is the one source of truth; `routes` is derived and rebuilt at startup, per spec §6.

**Files:**
- Create: `crates/balerix-plugin-matrix/src/routing.rs`
- Modify: `crates/balerix-plugin-matrix/src/lib.rs`

**Interfaces:**
- Consumes: `balerix_plugin_sdk::{Host, SdkError}`.
- Produces: `Thread { session_id, root, room, closed }`. `crew_of(agent: &str) -> Option<&str>`. `room_key(crew) -> String`, `thread_key(agent) -> String`, `THREAD_PREFIX`, `ROOM_PREFIX`. `Maps` with `new`, `load(&Host)`, `room(crew)`, `set_room(&Host, crew, room)`, `thread(agent)`, `set_thread(&Host, agent, Thread)`, `close_thread(&Host, agent)`, `forget(&Host, agent)`, `route(room, root)`, `rooms_len()`, `open_threads()`.

- [ ] **Step 1: Write the failing tests**

Create `crates/balerix-plugin-matrix/src/routing.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use balerix_plugin_sdk::testing::FakeHost;

    fn thread(session: &str, root: &str) -> Thread {
        Thread {
            session_id: session.into(),
            root: root.into(),
            room: "!r:fake".into(),
            closed: false,
        }
    }

    async fn host() -> (FakeHost, Host) {
        let fake = FakeHost::start("tok", serde_json::json!({}), Vec::new()).await;
        let env = fake.env("matrix", std::path::Path::new("scratch"));
        let host = Host::new(env).unwrap();
        (fake, host)
    }

    #[test]
    fn a_crew_is_an_agent_id_without_its_last_segment() {
        assert_eq!(crew_of("payments/backend/alice"), Some("payments/backend"));
        assert_eq!(crew_of("payments/backend"), None);
        assert_eq!(crew_of("alice"), None);
        assert_eq!(room_key("payments/backend"), "room/payments/backend");
        assert_eq!(
            thread_key("payments/backend/alice"),
            "thread/payments/backend/alice"
        );
    }

    #[tokio::test]
    async fn rooms_and_threads_survive_a_reload_and_routes_are_rebuilt() {
        let (_fake, host) = host().await;
        let mut maps = Maps::new();
        maps.set_room(&host, "payments/backend", "!r:fake")
            .await
            .unwrap();
        maps.set_thread(&host, "payments/backend/alice", thread("s1", "$root1"))
            .await
            .unwrap();
        assert_eq!(maps.room("payments/backend"), Some("!r:fake"));
        assert_eq!(
            maps.route("!r:fake", "$root1"),
            Some("payments/backend/alice")
        );
        assert_eq!(maps.rooms_len(), 1);
        assert_eq!(maps.open_threads(), 1);

        let reloaded = Maps::load(&host).await.unwrap();
        assert_eq!(reloaded.room("payments/backend"), Some("!r:fake"));
        assert_eq!(
            reloaded.route("!r:fake", "$root1"),
            Some("payments/backend/alice"),
            "routes are derived from the stored threads"
        );
        assert_eq!(
            reloaded.thread("payments/backend/alice").map(|t| t.session_id.as_str()),
            Some("s1")
        );
    }

    #[tokio::test]
    async fn a_new_session_replaces_the_old_thread_and_its_route() {
        let (_fake, host) = host().await;
        let mut maps = Maps::new();
        maps.set_thread(&host, "f/c/a", thread("s1", "$root1"))
            .await
            .unwrap();
        maps.set_thread(&host, "f/c/a", thread("s2", "$root2"))
            .await
            .unwrap();
        assert_eq!(maps.route("!r:fake", "$root2"), Some("f/c/a"));
        assert_eq!(
            maps.route("!r:fake", "$root1"),
            None,
            "the previous session's root stops routing"
        );
        assert_eq!(maps.open_threads(), 1);
    }

    #[tokio::test]
    async fn closing_keeps_the_thread_and_forgetting_removes_it() {
        let (fake, host) = host().await;
        let mut maps = Maps::new();
        maps.set_thread(&host, "f/c/a", thread("s1", "$root1"))
            .await
            .unwrap();
        maps.close_thread(&host, "f/c/a").await.unwrap();
        assert!(maps.thread("f/c/a").is_some_and(|t| t.closed));
        assert_eq!(maps.open_threads(), 0, "a closed thread is not open");
        assert_eq!(
            maps.route("!r:fake", "$root1"),
            Some("f/c/a"),
            "still resolvable, so a reply can be told the session ended"
        );
        assert!(fake.kv().contains_key("thread/f/c/a"));

        maps.forget(&host, "f/c/a").await.unwrap();
        assert!(maps.thread("f/c/a").is_none());
        assert_eq!(maps.route("!r:fake", "$root1"), None);
        assert!(!fake.kv().contains_key("thread/f/c/a"));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix -E 'test(routing)'`
Expected: FAIL to compile, `cannot find type 'Thread'`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/balerix-plugin-matrix/src/routing.rs`:

```rust
//! The room, thread and route maps (Spec G §6, §7). `threads` is the one
//! source of truth and is mirrored to the daemon's KV, so a restart
//! resumes; `routes` is derived from it and rebuilt at startup.

use std::collections::HashMap;

use balerix_plugin_sdk::{Host, SdkError};
use serde::{Deserialize, Serialize};

pub const ROOM_PREFIX: &str = "room/";
pub const THREAD_PREFIX: &str = "thread/";

/// One agent session's thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub session_id: String,
    /// The thread root's event id.
    pub root: String,
    pub room: String,
    #[serde(default)]
    pub closed: bool,
}

/// `fleet/crew/agent` to `fleet/crew`; `None` for anything else, which the
/// daemon never produces.
pub fn crew_of(agent: &str) -> Option<&str> {
    let crew = agent.rfind('/').map(|i| &agent[..i])?;
    crew.contains('/').then_some(crew)
}

pub fn room_key(crew: &str) -> String {
    format!("{ROOM_PREFIX}{crew}")
}

pub fn thread_key(agent: &str) -> String {
    format!("{THREAD_PREFIX}{agent}")
}

#[derive(Debug, Default)]
pub struct Maps {
    rooms: HashMap<String, String>,
    threads: HashMap<String, Thread>,
    routes: HashMap<(String, String), String>,
}

impl Maps {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuilds from KV. A record that no longer parses is dropped with a
    /// log line rather than failing startup: the plugin's job is to keep
    /// posting, and the next `SessionStart` re-creates the thread.
    pub async fn load(host: &Host) -> Result<Self, SdkError> {
        let mut maps = Self::new();
        for key in host.kv_list(ROOM_PREFIX).await? {
            let Some(bytes) = host.kv_get(&key).await? else {
                continue;
            };
            match String::from_utf8(bytes) {
                Ok(room) => {
                    maps.rooms.insert(key[ROOM_PREFIX.len()..].to_string(), room);
                }
                Err(e) => tracing::warn!("matrix: bad room record {key}: {e}"),
            }
        }
        for key in host.kv_list(THREAD_PREFIX).await? {
            let Some(bytes) = host.kv_get(&key).await? else {
                continue;
            };
            match serde_json::from_slice::<Thread>(&bytes) {
                Ok(thread) => {
                    let agent = key[THREAD_PREFIX.len()..].to_string();
                    maps.insert_thread(agent, thread);
                }
                Err(e) => tracing::warn!("matrix: bad thread record {key}: {e}"),
            }
        }
        Ok(maps)
    }

    fn insert_thread(&mut self, agent: String, thread: Thread) {
        if let Some(old) = self.threads.get(&agent) {
            self.routes.remove(&(old.room.clone(), old.root.clone()));
        }
        self.routes
            .insert((thread.room.clone(), thread.root.clone()), agent.clone());
        self.threads.insert(agent, thread);
    }

    pub fn room(&self, crew: &str) -> Option<&str> {
        self.rooms.get(crew).map(String::as_str)
    }

    pub async fn set_room(
        &mut self,
        host: &Host,
        crew: &str,
        room: &str,
    ) -> Result<(), SdkError> {
        host.kv_put(&room_key(crew), room.as_bytes(), false).await?;
        self.rooms.insert(crew.to_string(), room.to_string());
        Ok(())
    }

    pub fn thread(&self, agent: &str) -> Option<&Thread> {
        self.threads.get(agent)
    }

    pub async fn set_thread(
        &mut self,
        host: &Host,
        agent: &str,
        thread: Thread,
    ) -> Result<(), SdkError> {
        let bytes = serde_json::to_vec(&thread)
            .map_err(|e| SdkError::Transport(format!("encode thread: {e}")))?;
        host.kv_put(&thread_key(agent), &bytes, false).await?;
        self.insert_thread(agent.to_string(), thread);
        Ok(())
    }

    /// Marks the session ended. The record stays so a late reply can be
    /// told why it was not delivered (Spec G-12).
    pub async fn close_thread(&mut self, host: &Host, agent: &str) -> Result<(), SdkError> {
        let Some(mut thread) = self.threads.get(agent).cloned() else {
            return Ok(());
        };
        thread.closed = true;
        self.set_thread(host, agent, thread).await
    }

    /// Drops the agent entirely: `deactivate`.
    pub async fn forget(&mut self, host: &Host, agent: &str) -> Result<(), SdkError> {
        if let Some(old) = self.threads.remove(agent) {
            self.routes.remove(&(old.room, old.root));
        }
        host.kv_delete(&thread_key(agent)).await
    }

    pub fn route(&self, room: &str, root: &str) -> Option<&str> {
        self.routes
            .get(&(room.to_string(), root.to_string()))
            .map(String::as_str)
    }

    pub fn rooms_len(&self) -> usize {
        self.rooms.len()
    }

    pub fn open_threads(&self) -> usize {
        self.threads.values().filter(|t| !t.closed).count()
    }
}
```

Add `pub mod routing;` to `src/lib.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix`
Expected: PASS.

- [ ] **Step 5: Run the full gate and commit**

Run: `mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log`

```bash
git add crates/balerix-plugin-matrix
git commit -m "Persist the matrix plugin's room and thread maps in the daemon KV"
```

---
### Task 8: The command queue, the counters and the health cell

The plumbing the actor sits on. The queue is drop-oldest at 1024, matching `balerix_api::OBSERVER_QUEUE`, so a slow homeserver can never stall hook delivery to Claude (spec G-11). `tokio::sync::mpsc` cannot drop its oldest entry, so the queue is a deque behind a mutex with a `Notify`.

**Files:**
- Create: `crates/balerix-plugin-matrix/src/actor.rs`
- Modify: `crates/balerix-plugin-matrix/src/lib.rs`
- Modify: `crates/balerix-plugin-matrix/Cargo.toml`

**Interfaces:**
- Consumes: `config::{AgentConfig, DaemonConfig}`, `matrix::Inbound`, `render::PhaseChange`, `balerix_plugin_sdk::metrics::{IntCounter, IntCounterVec, IntGauge}`.
- Produces: `Command` with variants `Configure(DaemonConfig)`, `Activate { agent: String, config: AgentConfig }`, `Deactivate { agent: String }`, `Events(Vec<HookEvent>)`, `Phases(Vec<PhaseChange>)`, `Inbound(Inbound)`. `QUEUE: usize`. `Queue::{new, push, pop, len}`. `Counters::new(&Metrics) -> Result<Counters, SdkError>` with fields `messages_sent`, `events_dropped`, `inbound`, `rooms`, `threads_open`, `errors`. `Health::{new, ok, fail, get}`.

- [ ] **Step 1: Add the `tokio` sync feature check**

`tokio` in `[workspace.dependencies]` already carries `sync`, `time`, `rt-multi-thread`, `macros`, `signal` and `net`. Nothing to add. Confirm with:

Run: `grep -n '^tokio' Cargo.toml`
Expected: the features list contains `sync` and `time`.

- [ ] **Step 2: Write the failing tests**

Create `crates/balerix-plugin-matrix/src/actor.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use balerix_plugin_sdk::Metrics;

    fn counters() -> Counters {
        Counters::new(&Metrics::new("matrix")).unwrap()
    }

    fn deactivate(agent: &str) -> Command {
        Command::Deactivate {
            agent: agent.to_string(),
        }
    }

    #[tokio::test]
    async fn the_queue_is_fifo_and_wakes_a_waiting_pop() {
        let c = counters();
        let q = Queue::new(c.events_dropped.clone());
        let popper = {
            let q = q.clone();
            tokio::spawn(async move { q.pop().await })
        };
        q.push(deactivate("f/c/a"));
        assert_eq!(popper.await.unwrap(), deactivate("f/c/a"));

        q.push(deactivate("one"));
        q.push(deactivate("two"));
        assert_eq!(q.pop().await, deactivate("one"));
        assert_eq!(q.pop().await, deactivate("two"));
        assert_eq!(q.len(), 0);
    }

    #[tokio::test]
    async fn a_full_queue_drops_the_oldest_and_counts_it() {
        let c = counters();
        let q = Queue::new(c.events_dropped.clone());
        for i in 0..QUEUE {
            q.push(deactivate(&format!("a{i}")));
        }
        assert_eq!(q.len(), QUEUE);
        assert_eq!(c.events_dropped.get(), 0);

        q.push(deactivate("newest"));
        assert_eq!(q.len(), QUEUE, "capacity is held");
        assert_eq!(c.events_dropped.get(), 1);
        assert_eq!(
            q.pop().await,
            deactivate("a1"),
            "the oldest was dropped, not the newest"
        );
    }

    #[test]
    fn health_starts_ok_and_reports_the_last_failure_until_cleared() {
        let h = Health::new();
        assert_eq!(h.get(), Ok(()));
        h.fail("create room for f/c: no rights".into());
        assert_eq!(h.get(), Err("create room for f/c: no rights".into()));
        h.ok();
        assert_eq!(h.get(), Ok(()));
    }

    #[test]
    fn every_metric_family_carries_the_plugin_prefix() {
        let m = Metrics::new("matrix");
        let c = Counters::new(&m).unwrap();
        c.messages_sent.with_label_values(&["event"]).inc();
        c.inbound.with_label_values(&["routed"]).inc();
        c.errors.with_label_values(&["send"]).inc();
        c.rooms.set(2);
        c.threads_open.set(3);
        c.events_dropped.inc();
        let text = m.render().unwrap();
        for family in [
            "balerix_plugin_matrix_messages_sent_total",
            "balerix_plugin_matrix_events_dropped_total",
            "balerix_plugin_matrix_inbound_total",
            "balerix_plugin_matrix_rooms",
            "balerix_plugin_matrix_threads_open",
            "balerix_plugin_matrix_errors_total",
        ] {
            assert!(text.contains(family), "missing {family} in\n{text}");
        }
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix -E 'test(actor)'`
Expected: FAIL to compile, `cannot find type 'Counters'`.

- [ ] **Step 4: Write the implementation**

Prepend to `crates/balerix-plugin-matrix/src/actor.rs`:

```rust
//! The one task that owns every piece of mutable state (Spec G-10), the
//! bounded drop-oldest queue that feeds it (G-11), and the counters and
//! health cell it publishes through.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use balerix_api::{HookEvent, OBSERVER_QUEUE};
use balerix_plugin_sdk::metrics::{IntCounter, IntCounterVec, IntGauge};
use balerix_plugin_sdk::{Metrics, SdkError};
use tokio::sync::Notify;

use crate::config::{AgentConfig, DaemonConfig};
use crate::matrix::Inbound;
use crate::render::PhaseChange;

/// Queue depth, the same as the daemon's own observer queues.
pub const QUEUE: usize = OBSERVER_QUEUE;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Configure(DaemonConfig),
    Activate { agent: String, config: AgentConfig },
    Deactivate { agent: String },
    Events(Vec<HookEvent>),
    Phases(Vec<PhaseChange>),
    Inbound(Inbound),
}

/// A bounded queue that drops its oldest entry rather than blocking its
/// producer: `observe` is a daemon-to-plugin HTTP call and must return.
pub struct Queue {
    inner: Mutex<VecDeque<Command>>,
    notify: Notify,
    dropped: IntCounter,
}

impl Queue {
    pub fn new(dropped: IntCounter) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(VecDeque::with_capacity(QUEUE)),
            notify: Notify::new(),
            dropped,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Command>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn push(&self, command: Command) {
        {
            let mut q = self.lock();
            if q.len() >= QUEUE {
                q.pop_front();
                self.dropped.inc();
            }
            q.push_back(command);
        }
        // `notify_one` stores a permit when nobody is waiting, so a pop
        // that arrives afterwards returns at once: no lost wakeups.
        self.notify.notify_one();
    }

    pub async fn pop(&self) -> Command {
        loop {
            if let Some(command) = self.lock().pop_front() {
                return command;
            }
            self.notify.notified().await;
        }
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The metric families of Spec G §10.
#[derive(Debug, Clone)]
pub struct Counters {
    pub messages_sent: IntCounterVec,
    pub events_dropped: IntCounter,
    pub inbound: IntCounterVec,
    pub rooms: IntGauge,
    pub threads_open: IntGauge,
    pub errors: IntCounterVec,
}

impl Counters {
    pub fn new(metrics: &Metrics) -> Result<Self, SdkError> {
        Ok(Self {
            messages_sent: metrics.int_counter_vec(
                "messages_sent_total",
                "Messages sent to Matrix, by kind",
                &["kind"],
            )?,
            events_dropped: metrics.int_counter(
                "events_dropped_total",
                "Commands dropped because the queue was full",
            )?,
            inbound: metrics.int_counter_vec(
                "inbound_total",
                "Matrix messages seen, by what became of them",
                &["outcome"],
            )?,
            rooms: metrics.int_gauge("rooms", "Crew rooms the plugin knows")?,
            threads_open: metrics.int_gauge("threads_open", "Agent sessions with an open thread")?,
            errors: metrics.int_counter_vec(
                "errors_total",
                "Matrix failures, by kind",
                &["kind"],
            )?,
        })
    }
}

/// What `Plugin::health` reports. The actor writes it; the plugin reads it.
#[derive(Debug, Clone, Default)]
pub struct Health(Arc<Mutex<Option<String>>>);

impl Health {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn ok(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    pub fn fail(&self, message: String) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
    }
    pub fn get(&self) -> Result<(), String> {
        match self.0.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            Some(m) => Err(m),
            None => Ok(()),
        }
    }
}
```

Add `pub mod actor;` to `src/lib.rs`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix`
Expected: PASS.

- [ ] **Step 6: Run the full gate and commit**

Run: `mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log`

```bash
git add crates/balerix-plugin-matrix
git commit -m "Give the matrix plugin a drop-oldest queue, counters and a health cell"
```

---

### Task 9: The actor, outbound

Rooms, thread lifecycle and event posting. This is where the ordering rules of spec §13 are enforced and tested: a root exists before any child, one room per crew under concurrent activation, a same-id `SessionStart` reuses the thread.

**Files:**
- Modify: `crates/balerix-plugin-matrix/src/actor.rs`

**Interfaces:**
- Consumes: `Counters`, `Health`, `Command`, `Queue` from Task 8; `MatrixPort`, `MatrixError` from Task 6; `Maps`, `Thread`, `crew_of` from Task 7; `render` from Task 5.
- Produces: `Actor<M: MatrixPort>` with `new(host: Host, port: M, counters: Counters, health: Health) -> Actor<M>`, `load(&mut self)` to restore the maps from KV, `handle(&mut self, command: Command)`, and `run(self, queue: Arc<Queue>)` which loops forever.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/balerix-plugin-matrix/src/actor.rs`:

```rust
    use crate::matrix::fake::{Call, FakePort};
    use crate::routing::Maps;
    use balerix_plugin_sdk::testing::{FakeHost, event};
    use balerix_plugin_sdk::Host;
    use serde_json::json;

    fn daemon_config() -> DaemonConfig {
        crate::config::parse_daemon(&json!({
            "homeserver": "https://h",
            "userId": "@balerix:example.org",
            "password": "pw",
            "invite": ["@rahul:example.org"]
        }))
        .unwrap()
    }

    fn agent_config(events: &[&str]) -> AgentConfig {
        crate::config::parse_agent(&json!({ "events": events })).unwrap()
    }

    fn started(agent: &str, session: &str, source: &str) -> HookEvent {
        let mut e = event(agent, "SessionStart", json!({ "source": source }));
        e.session_id = Some(session.to_string());
        e
    }

    fn during(agent: &str, session: &str, name: &str, payload: serde_json::Value) -> HookEvent {
        let mut e = event(agent, name, payload);
        e.session_id = Some(session.to_string());
        e
    }

    async fn actor() -> (FakeHost, FakePort, Actor<FakePort>) {
        let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
        let host = Host::new(fake.env("matrix", std::path::Path::new("scratch"))).unwrap();
        let port = FakePort::new("@balerix:example.org");
        let a = Actor::new(host, port.clone(), counters(), Health::new());
        (fake, port, a)
    }

    fn sends(calls: &[Call]) -> Vec<(Option<String>, String)> {
        calls
            .iter()
            .filter_map(|c| match c {
                Call::Send {
                    thread_root, body, ..
                } => Some((thread_root.clone(), body.clone())),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn commands_before_configure_are_buffered_and_replayed_in_order() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        assert!(port.calls().is_empty(), "nothing before configure");

        a.handle(Command::Configure(daemon_config())).await;
        let calls = port.calls();
        assert!(
            matches!(calls.first(), Some(Call::CreateRoom { .. })),
            "the room comes first: {calls:?}"
        );
        assert_eq!(sends(&calls).len(), 1, "then the thread root");
        assert_eq!(sends(&calls)[0].0, None, "the root is not a thread reply");
    }

    #[tokio::test]
    async fn two_agents_in_one_crew_share_a_single_room() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        for name in ["alice", "bob"] {
            a.handle(Command::Activate {
                agent: format!("f/c/{name}"),
                config: agent_config(&["Notification"]),
            })
            .await;
            a.handle(Command::Events(vec![started(
                &format!("f/c/{name}"),
                "s1",
                "startup",
            )]))
            .await;
        }
        let rooms = port
            .calls()
            .iter()
            .filter(|c| matches!(c, Call::CreateRoom { .. }))
            .count();
        assert_eq!(rooms, 1, "one room per crew");
        assert_eq!(sends(&port.calls()).len(), 2, "one root per agent");
    }

    #[tokio::test]
    async fn a_root_exists_before_any_child_and_children_are_thread_replies() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        a.handle(Command::Events(vec![
            started("f/c/alice", "s1", "startup"),
            during(
                "f/c/alice",
                "s1",
                "Notification",
                json!({ "message": "needs permission" }),
            ),
        ]))
        .await;

        let s = sends(&port.calls());
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].0, None, "root first");
        assert!(s[0].1.contains("session"), "{}", s[0].1);
        assert!(s[1].0.is_some(), "the child is a thread reply");
        assert!(s[1].1.contains("needs permission"), "{}", s[1].1);
    }

    #[tokio::test]
    async fn a_same_id_session_start_reuses_the_thread_and_a_new_id_opens_another() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        port.take_calls();

        a.handle(Command::Events(vec![started("f/c/alice", "s1", "compact")]))
            .await;
        let s = sends(&port.take_calls());
        assert_eq!(s.len(), 1);
        assert!(s[0].0.is_some(), "a compaction posts inside the thread");
        assert!(s[0].1.contains("restarted"), "{}", s[0].1);

        a.handle(Command::Events(vec![started("f/c/alice", "s2", "clear")]))
            .await;
        let s = sends(&port.take_calls());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0, None, "a new session id opens a new root");
    }

    #[tokio::test]
    async fn session_end_closes_the_thread_and_the_filter_drops_unwanted_events() {
        let (fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        a.handle(Command::Events(vec![
            started("f/c/alice", "s1", "startup"),
            during(
                "f/c/alice",
                "s1",
                "PreToolUse",
                json!({ "tool_name": "Bash" }),
            ),
            during("f/c/alice", "s1", "SessionEnd", json!({ "reason": "clear" })),
        ]))
        .await;

        let bodies: Vec<String> = sends(&port.calls()).into_iter().map(|(_, b)| b).collect();
        assert_eq!(bodies.len(), 2, "PreToolUse is filtered out: {bodies:?}");
        assert!(bodies[1].contains("session ended"), "{}", bodies[1]);

        let stored = fake.kv_json("thread/f/c/alice").unwrap();
        assert_eq!(stored["closed"], true);
    }

    #[tokio::test]
    async fn a_disabled_agent_and_an_unknown_agent_post_nothing() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: crate::config::parse_agent(&json!({ "enabled": false })).unwrap(),
        })
        .await;
        a.handle(Command::Events(vec![
            started("f/c/alice", "s1", "startup"),
            started("f/c/ghost", "s1", "startup"),
        ]))
        .await;
        assert!(port.calls().is_empty(), "{:?}", port.calls());
    }

    #[tokio::test]
    async fn a_pinned_room_is_used_instead_of_creating_one() {
        let (_fake, port, mut a) = actor().await;
        let mut cfg = daemon_config();
        cfg.rooms.insert("f/c".into(), "!pinned:example.org".into());
        a.handle(Command::Configure(cfg)).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&[]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;

        assert!(
            !port.calls().iter().any(|c| matches!(c, Call::CreateRoom { .. })),
            "no room was created"
        );
        assert!(matches!(
            port.calls().first(),
            Some(Call::Send { room, .. }) if room == "!pinned:example.org"
        ));
    }

    #[tokio::test]
    async fn a_failed_room_creation_is_reported_and_retried_on_the_next_event() {
        let (_fake, port, mut a) = actor().await;
        let health = Health::new();
        a.set_health(health.clone());
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;

        port.fail_next(crate::matrix::MatrixError::Other("no rights".into()));
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        assert!(health.get().is_err(), "the failure is visible");
        assert!(sends(&port.calls()).is_empty());

        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        assert_eq!(sends(&port.calls()).len(), 1, "the next event retries");
        assert_eq!(health.get(), Ok(()), "and clears the failure");
    }

    #[tokio::test]
    async fn a_rate_limited_send_is_retried_once() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&[]),
        })
        .await;
        port.fail_next(crate::matrix::MatrixError::RateLimited { retry_after_ms: 1 });
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        assert_eq!(sends(&port.calls()).len(), 1, "the retry got through");
    }

    #[tokio::test]
    async fn a_phase_change_posts_into_the_thread_when_there_is_one() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&[]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        port.take_calls();
        a.handle(Command::Phases(vec![PhaseChange {
            agent: "f/c/alice".into(),
            from: balerix_api::AgentPhase::Ready,
            to: balerix_api::AgentPhase::Failed,
            message: "window gone".into(),
        }]))
        .await;
        let s = sends(&port.calls());
        assert_eq!(s.len(), 1);
        assert!(s[0].0.is_some(), "in the thread");
        assert!(s[0].1.contains("window gone"), "{}", s[0].1);
    }

    #[tokio::test]
    async fn deactivate_forgets_the_agent_and_its_thread() {
        let (fake, _port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&[]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        assert!(fake.kv().contains_key("thread/f/c/alice"));
        a.handle(deactivate("f/c/alice")).await;
        assert!(!fake.kv().contains_key("thread/f/c/alice"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix -E 'test(actor)'`
Expected: FAIL to compile, `cannot find type 'Actor'`.

- [ ] **Step 3: Write the implementation**

Add to `crates/balerix-plugin-matrix/src/actor.rs`, after `Health`:

```rust
use std::collections::HashMap;
use std::time::Duration;

use balerix_plugin_sdk::Host;
use serde_json::Value;

use crate::matrix::{MatrixError, MatrixPort};
use crate::render;
use crate::routing::{Maps, Thread, crew_of};

/// The one owner of every mutable piece of state (Spec G-10). Generic over
/// the port rather than holding a trait object, because the port's methods
/// return `impl Future` and so are not dyn-compatible (G-13).
pub struct Actor<M: MatrixPort> {
    host: Host,
    port: M,
    counters: Counters,
    health: Health,
    config: Option<DaemonConfig>,
    /// Commands that arrived before `Configure`. The daemon may `activate`
    /// before the SDK's `configure` returns, so nothing may be dropped.
    pending: Vec<Command>,
    agents: HashMap<String, AgentConfig>,
    maps: Maps,
}

impl<M: MatrixPort> Actor<M> {
    pub fn new(host: Host, port: M, counters: Counters, health: Health) -> Self {
        Self {
            host,
            port,
            counters,
            health,
            config: None,
            pending: Vec::new(),
            agents: HashMap::new(),
            maps: Maps::new(),
        }
    }

    /// Replaces the health cell. Only the wiring and its tests use this.
    pub fn set_health(&mut self, health: Health) {
        self.health = health;
    }

    /// Restores the room and thread maps from KV, so a restart resumes.
    pub async fn load(&mut self) {
        match Maps::load(&self.host).await {
            Ok(maps) => self.maps = maps,
            Err(e) => tracing::warn!("matrix: loading maps: {e}"),
        }
        self.publish_gauges();
    }

    pub async fn run(mut self, queue: Arc<Queue>) {
        loop {
            let command = queue.pop().await;
            self.handle(command).await;
        }
    }

    pub async fn handle(&mut self, command: Command) {
        if let Command::Configure(config) = command {
            self.config = Some(config);
            for buffered in std::mem::take(&mut self.pending) {
                Box::pin(self.handle(buffered)).await;
            }
            return;
        }
        if self.config.is_none() {
            self.pending.push(command);
            return;
        }
        match command {
            Command::Configure(_) => unreachable!("handled above"),
            Command::Activate { agent, config } => {
                self.agents.insert(agent, config);
            }
            Command::Deactivate { agent } => {
                self.agents.remove(&agent);
                if let Err(e) = self.maps.forget(&self.host, &agent).await {
                    tracing::warn!("matrix: forgetting {agent}: {e}");
                }
                self.publish_gauges();
            }
            Command::Events(events) => {
                for event in events {
                    self.on_event(event).await;
                }
            }
            Command::Phases(changes) => {
                for change in changes {
                    self.on_phase(change).await;
                }
            }
            Command::Inbound(message) => self.on_inbound(message).await,
        }
    }

    fn publish_gauges(&self) {
        self.counters.rooms.set(self.maps.rooms_len() as i64);
        self.counters
            .threads_open
            .set(self.maps.open_threads() as i64);
    }

    /// The crew's room: known, then pinned, then created. `None` means the
    /// homeserver refused, which is reported and retried on the next event.
    async fn room_for(&mut self, agent: &str) -> Option<String> {
        let crew = crew_of(agent)?.to_string();
        if let Some(room) = self.maps.room(&crew) {
            return Some(room.to_string());
        }
        let (pinned, invite) = {
            let config = self.config.as_ref()?;
            (config.rooms.get(&crew).cloned(), config.invite.clone())
        };
        let room = match pinned {
            Some(room) => room,
            None => match self
                .port
                .create_room(&format!("balerix {crew}"), &invite)
                .await
            {
                Ok(room) => room,
                Err(e) => {
                    self.counters.errors.with_label_values(&["create_room"]).inc();
                    self.health.fail(format!("create room for {crew}: {e}"));
                    tracing::warn!("matrix: create room for {crew}: {e}");
                    return None;
                }
            },
        };
        if let Err(e) = self.maps.set_room(&self.host, &crew, &room).await {
            tracing::warn!("matrix: storing room for {crew}: {e}");
        }
        self.health.ok();
        self.publish_gauges();
        Some(room)
    }

    /// One send, retried once on a rate limit with the server's own delay.
    async fn send(
        &self,
        room: &str,
        thread_root: Option<&str>,
        body: &str,
        kind: &str,
    ) -> Option<String> {
        let mut attempt = self.port.send(room, thread_root, body).await;
        if let Err(MatrixError::RateLimited { retry_after_ms }) = attempt {
            tokio::time::sleep(Duration::from_millis(retry_after_ms)).await;
            attempt = self.port.send(room, thread_root, body).await;
        }
        match attempt {
            Ok(id) => {
                self.counters.messages_sent.with_label_values(&[kind]).inc();
                Some(id)
            }
            Err(e) => {
                self.counters.errors.with_label_values(&["send"]).inc();
                tracing::warn!("matrix: send to {room}: {e}");
                None
            }
        }
    }

    async fn on_event(&mut self, event: HookEvent) {
        let Some(config) = self.agents.get(&event.agent).cloned() else {
            return;
        };
        if !config.enabled {
            return;
        }
        let Some(room) = self.room_for(&event.agent).await else {
            return;
        };
        let session = event.session_id.clone().unwrap_or_default();

        // A thread is opened for a new session id, and for an agent we
        // have no thread for at all: the plugin may have started mid
        // session, and an event must never be dropped for want of a root.
        let opening = match self.maps.thread(&event.agent) {
            None => true,
            Some(thread) => !session.is_empty() && thread.session_id != session,
        };
        if opening {
            let source = event
                .payload
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("already running");
            let body = render::thread_root(&event.agent, &session, source);
            let Some(root) = self.send(&room, None, &body, "root").await else {
                return;
            };
            let thread = Thread {
                session_id: session,
                root,
                room: room.clone(),
                closed: false,
            };
            if let Err(e) = self.maps.set_thread(&self.host, &event.agent, thread).await {
                tracing::warn!("matrix: storing thread for {}: {e}", event.agent);
            }
            self.publish_gauges();
            // The root already says the session started.
            if event.name == "SessionStart" {
                return;
            }
        }

        if !config.wants(&event.name) {
            return;
        }
        let Some(root) = self.maps.thread(&event.agent).map(|t| t.root.clone()) else {
            return;
        };
        let body = render::event_message(&event);
        self.send(&room, Some(&root), &body, "event").await;

        if event.name == "SessionEnd" {
            if let Err(e) = self.maps.close_thread(&self.host, &event.agent).await {
                tracing::warn!("matrix: closing thread for {}: {e}", event.agent);
            }
            self.publish_gauges();
        }
    }

    async fn on_phase(&mut self, change: PhaseChange) {
        let Some(config) = self.agents.get(&change.agent).cloned() else {
            return;
        };
        if !config.enabled || !config.phases {
            return;
        }
        let Some(room) = self.room_for(&change.agent).await else {
            return;
        };
        let root = self.maps.thread(&change.agent).map(|t| t.root.clone());
        let body = render::phase_message(&change);
        self.send(&room, root.as_deref(), &body, "phase").await;
    }

    /// Task 10.
    async fn on_inbound(&mut self, _message: Inbound) {}
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix`
Expected: PASS.

- [ ] **Step 5: Run the full gate and commit**

Run: `mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log`

```bash
git add crates/balerix-plugin-matrix
git commit -m "Post agent events to a room per crew and a thread per session"
```

---
### Task 10: The actor, inbound

A thread reply becomes a `send_text`. Every rejection is counted under its own outcome label, which is what makes this debuggable, and the ones a human should see get a reaction.

**Files:**
- Modify: `crates/balerix-plugin-matrix/src/actor.rs`

**Interfaces:**
- Consumes: `matrix::{ACK, FAILED, REFUSED, Inbound}`, `balerix_api::PluginAction`, `Host::action`.
- Produces: `Actor::on_inbound` fully implemented. No new public names.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/balerix-plugin-matrix/src/actor.rs`:

```rust
    use crate::matrix::{ACK, FAILED, REFUSED};
    use balerix_api::PluginAction;

    fn inbound(room: &str, root: Option<&str>, sender: &str, body: &str) -> Inbound {
        Inbound {
            room: room.to_string(),
            event_id: "$msg:fake".into(),
            sender: sender.to_string(),
            thread_root: root.map(str::to_string),
            body: body.to_string(),
        }
    }

    fn reactions(calls: &[Call]) -> Vec<String> {
        calls
            .iter()
            .filter_map(|c| match c {
                Call::React { key, .. } => Some(key.clone()),
                _ => None,
            })
            .collect()
    }

    /// An actor with one live thread; returns the room and its root.
    async fn with_thread() -> (FakeHost, FakePort, Actor<FakePort>, String, String) {
        let (fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&[]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        let (room, root) = match port.calls().last() {
            Some(Call::Send { room, .. }) => (room.clone(), "$evt2:fake".to_string()),
            other => panic!("expected a root send, got {other:?}"),
        };
        port.take_calls();
        (fake, port, a, room, root)
    }

    #[tokio::test]
    async fn a_thread_reply_becomes_a_submitted_send_text_and_is_acknowledged() {
        let (fake, port, mut a, room, root) = with_thread().await;
        a.handle(Command::Inbound(inbound(
            &room,
            Some(&root),
            "@rahul:example.org",
            "run the tests",
        )))
        .await;
        assert_eq!(
            fake.actions_for("f/c/alice"),
            vec![PluginAction::SendText {
                text: "run the tests".into(),
                submit: true,
            }]
        );
        assert_eq!(reactions(&port.calls()), vec![ACK.to_string()]);
    }

    #[tokio::test]
    async fn our_own_message_is_ignored_without_a_reaction() {
        let (fake, port, mut a, room, root) = with_thread().await;
        a.handle(Command::Inbound(inbound(
            &room,
            Some(&root),
            "@balerix:example.org",
            "a message we sent",
        )))
        .await;
        assert!(fake.actions_for("f/c/alice").is_empty());
        assert!(port.calls().is_empty(), "no reaction on our own message");
    }

    #[tokio::test]
    async fn a_room_level_message_is_refused_and_an_unknown_thread_is_silent() {
        let (fake, port, mut a, room, _root) = with_thread().await;
        a.handle(Command::Inbound(inbound(
            &room,
            None,
            "@rahul:example.org",
            "hello room",
        )))
        .await;
        assert!(fake.actions_for("f/c/alice").is_empty());
        assert_eq!(reactions(&port.take_calls()), vec![REFUSED.to_string()]);

        a.handle(Command::Inbound(inbound(
            &room,
            Some("$someone-elses-thread"),
            "@rahul:example.org",
            "not ours",
        )))
        .await;
        assert!(
            port.calls().is_empty(),
            "a thread we do not own gets no reaction"
        );
    }

    #[tokio::test]
    async fn a_reply_in_a_closed_thread_is_refused() {
        let (fake, port, mut a, room, root) = with_thread().await;
        a.handle(Command::Events(vec![during(
            "f/c/alice",
            "s1",
            "SessionEnd",
            json!({ "reason": "clear" }),
        )]))
        .await;
        port.take_calls();

        a.handle(Command::Inbound(inbound(
            &room,
            Some(&root),
            "@rahul:example.org",
            "too late",
        )))
        .await;
        assert!(
            fake.actions_for("f/c/alice").is_empty(),
            "nothing reaches the agent"
        );
        assert_eq!(reactions(&port.calls()), vec![REFUSED.to_string()]);
    }

    #[tokio::test]
    async fn a_rejected_send_text_is_reported_in_the_thread() {
        let (fake, port, mut a, room, root) = with_thread().await;
        fake.fail_actions(Some("no such window"));
        a.handle(Command::Inbound(inbound(
            &room,
            Some(&root),
            "@rahul:example.org",
            "run the tests",
        )))
        .await;
        let calls = port.calls();
        assert_eq!(reactions(&calls), vec![FAILED.to_string()]);
        let notice = sends(&calls);
        assert_eq!(notice.len(), 1, "the failure is posted in the thread");
        assert_eq!(notice[0].0, Some(root), "in the thread, not the room");
        assert!(notice[0].1.contains("no such window"), "{}", notice[0].1);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix -E 'test(inbound) or test(own_message) or test(closed_thread)'`
Expected: FAIL, the actions list empty and no reactions, because `on_inbound` is still the Task 9 stub.

- [ ] **Step 3: Write the implementation**

In `crates/balerix-plugin-matrix/src/actor.rs`, add `use balerix_api::PluginAction;` and `use crate::matrix::{ACK, FAILED, REFUSED};` to the imports, then replace the `on_inbound` stub with:

```rust
    /// A reply in a live thread of ours becomes a `send_text` (Spec G §9).
    /// Every other shape is counted under its own outcome, and the ones a
    /// person should see get a reaction.
    async fn on_inbound(&mut self, message: Inbound) {
        let count = |outcome: &str| self.counters.inbound.with_label_values(&[outcome]).inc();

        if message.sender == self.port.user_id() {
            count("own_message");
            return;
        }
        let Some(root) = message.thread_root.clone() else {
            count("not_a_thread");
            self.react(&message, REFUSED).await;
            return;
        };
        let Some(agent) = self.maps.route(&message.room, &root).map(str::to_string) else {
            // A thread in a room we are in that is not one of ours. Silent
            // on purpose: reacting to every unrelated thread would be noise.
            count("unknown_thread");
            return;
        };
        if self.maps.thread(&agent).is_some_and(|t| t.closed) {
            count("stale_thread");
            self.react(&message, REFUSED).await;
            return;
        }

        let action = PluginAction::SendText {
            text: message.body.clone(),
            submit: true,
        };
        match self.host.action(&agent, &action).await {
            Ok(()) => {
                count("routed");
                self.react(&message, ACK).await;
            }
            Err(e) => {
                count("send_failed");
                self.counters.errors.with_label_values(&["send_text"]).inc();
                let body = format!("**not delivered to {agent}:** {e}");
                self.send(&message.room, Some(&root), &body, "notice").await;
                self.react(&message, FAILED).await;
            }
        }
    }

    async fn react(&self, message: &Inbound, key: &str) {
        if let Err(e) = self
            .port
            .react(&message.room, &message.event_id, key)
            .await
        {
            self.counters.errors.with_label_values(&["react"]).inc();
            tracing::warn!("matrix: reacting to {}: {e}", message.event_id);
        }
    }
```

The closure `count` borrows `self.counters` immutably while `self.react` and `self.send` take `&self`, which is fine, but `self.maps` reads are also immutable. If the borrow checker objects at a call site, replace the closure with a small method `fn count_inbound(&self, outcome: &str)`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix`
Expected: PASS.

- [ ] **Step 5: Run the full gate and commit**

Run: `mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log`

```bash
git add crates/balerix-plugin-matrix
git commit -m "Turn a live thread reply into a submitted send_text"
```

---

### Task 11: `session.rs`

The sealed session record and the pure restore-or-login decision. Keeping the decision out of the adapter is what makes "the password is read at most once" a tested property rather than a claim.

**Files:**
- Create: `crates/balerix-plugin-matrix/src/session.rs`
- Modify: `crates/balerix-plugin-matrix/src/config.rs` (add `Serialize` to `Secret`)
- Modify: `crates/balerix-plugin-matrix/src/lib.rs`

**Interfaces:**
- Consumes: `config::{DaemonConfig, Secret}`, `balerix_plugin_sdk::{Host, SdkError}`.
- Produces: `AUTH_KEY: &str = "auth"`. `Session { homeserver, user_id, device_id, access_token: Secret, refresh_token: Option<Secret> }` with a redacting `Debug`. `load(&Host) -> Result<Option<Session>, SdkError>`, `store(&Host, &Session) -> Result<(), SdkError>`, `clear(&Host) -> Result<(), SdkError>`. `Plan::{Restore(Box<Session>), Login { password: Secret }}`. `plan(Option<Session>, &DaemonConfig) -> Result<Plan, String>`. `REVOKED: &str`.

- [ ] **Step 1: Write the failing tests**

Create `crates/balerix-plugin-matrix/src/session.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use balerix_plugin_sdk::testing::FakeHost;
    use serde_json::json;

    fn config(password: Option<&str>) -> DaemonConfig {
        let mut v = json!({ "homeserver": "https://h", "userId": "@balerix:h" });
        if let Some(p) = password {
            v["password"] = json!(p);
        }
        crate::config::parse_daemon(&v).unwrap()
    }

    fn session() -> Session {
        Session {
            homeserver: "https://h".into(),
            user_id: "@balerix:h".into(),
            device_id: "balerix".into(),
            access_token: Secret::new("syt_tok"),
            refresh_token: Some(Secret::new("syr_ref")),
        }
    }

    #[test]
    fn a_matching_cached_session_is_restored_without_reading_the_password() {
        assert_eq!(
            plan(Some(session()), &config(Some("pw"))),
            Ok(Plan::Restore(Box::new(session())))
        );
        assert_eq!(
            plan(Some(session()), &config(None)),
            Ok(Plan::Restore(Box::new(session()))),
            "a cached session needs no password at all"
        );
    }

    #[test]
    fn a_changed_homeserver_or_user_discards_the_cached_session() {
        let mut c = config(Some("pw"));
        c.homeserver = "https://other".into();
        assert_eq!(
            plan(Some(session()), &c),
            Ok(Plan::Login {
                password: Secret::new("pw")
            })
        );
        let mut c = config(Some("pw"));
        c.user_id = "@other:h".into();
        assert!(matches!(plan(Some(session()), &c), Ok(Plan::Login { .. })));
    }

    #[test]
    fn no_cached_session_and_no_password_names_what_to_do() {
        let err = plan(None, &config(None)).unwrap_err();
        assert!(err.contains("no password"), "{err}");
        assert!(err.contains("secrets.password"), "names the file route: {err}");

        let mut c = config(None);
        c.homeserver = "https://other".into();
        let err = plan(Some(session()), &c).unwrap_err();
        assert!(err.contains("https://other"), "names the mismatch: {err}");
        assert!(!err.contains("syt_tok"), "never the token: {err}");
    }

    #[test]
    fn debug_never_prints_a_token() {
        let text = format!("{:?}", session());
        assert!(!text.contains("syt_tok") && !text.contains("syr_ref"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
        assert!(text.contains("balerix"), "the device id is readable: {text}");
    }

    #[tokio::test]
    async fn the_session_round_trips_through_sealed_kv() {
        let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
        let host = Host::new(fake.env("matrix", std::path::Path::new("scratch"))).unwrap();
        assert_eq!(load(&host).await.unwrap(), None);

        store(&host, &session()).await.unwrap();
        assert_eq!(load(&host).await.unwrap(), Some(session()));
        assert_eq!(
            fake.kv().get(AUTH_KEY).map(|(_, secret)| *secret),
            Some(true),
            "the record is stored sealed"
        );

        clear(&host).await.unwrap();
        assert_eq!(load(&host).await.unwrap(), None);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix -E 'test(session)'`
Expected: FAIL to compile, `cannot find type 'Session'`.

- [ ] **Step 3: Write the implementation**

In `crates/balerix-plugin-matrix/src/config.rs`, change `Secret`'s derive to include `Serialize`:

```rust
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);
```

and add `Serialize` to the `use serde::{Deserialize, Serialize};` line.

Prepend to `crates/balerix-plugin-matrix/src/session.rs`:

```rust
//! The cached Matrix session (Spec G §5.2). It lives in sealed KV, which
//! the daemon encrypts at rest with its vault key, and the decision about
//! whether to use it is a pure function so "the password is read at most
//! once" is a tested property.

use std::fmt;

use balerix_plugin_sdk::{Host, SdkError};
use serde::{Deserialize, Serialize};

use crate::config::{DaemonConfig, Secret};

/// The one sealed KV key this plugin writes.
pub const AUTH_KEY: &str = "auth";

/// What the operator should do when a cached session is unusable and no
/// password is configured.
pub const REVOKED: &str = "the cached session was rejected, most likely because its device was revoked; configure a password to log in again";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub homeserver: String,
    pub user_id: String,
    pub device_id: String,
    pub access_token: Secret,
    pub refresh_token: Option<Secret>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("homeserver", &self.homeserver)
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .finish()
    }
}

/// What to do at `configure`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Reuse the cached session; the password is never read.
    Restore(Box<Session>),
    Login { password: Secret },
}

/// A cached session is reused only when it belongs to the configured
/// homeserver and user. Otherwise a password is required, and its absence
/// is an error that says which of the two situations it is.
pub fn plan(cached: Option<Session>, config: &DaemonConfig) -> Result<Plan, String> {
    let usable = cached.filter(|s| s.homeserver == config.homeserver && s.user_id == config.user_id);
    if let Some(session) = usable {
        return Ok(Plan::Restore(Box::new(session)));
    }
    match &config.password {
        Some(password) => Ok(Plan::Login {
            password: password.clone(),
        }),
        None => Err(format!(
            "no cached session for {} at {}, and no password is configured: set `password` in the plugins.yaml config block, or add a `secrets.password` entry naming a 0600 file",
            config.user_id, config.homeserver
        )),
    }
}

pub async fn load(host: &Host) -> Result<Option<Session>, SdkError> {
    let Some(bytes) = host.kv_get(AUTH_KEY).await? else {
        return Ok(None);
    };
    match serde_json::from_slice(&bytes) {
        Ok(session) => Ok(Some(session)),
        Err(e) => {
            tracing::warn!("matrix: stored session is unreadable, logging in again: {e}");
            Ok(None)
        }
    }
}

pub async fn store(host: &Host, session: &Session) -> Result<(), SdkError> {
    let bytes = serde_json::to_vec(session)
        .map_err(|e| SdkError::Transport(format!("encode session: {e}")))?;
    host.kv_put(AUTH_KEY, &bytes, true).await
}

pub async fn clear(host: &Host) -> Result<(), SdkError> {
    host.kv_delete(AUTH_KEY).await
}
```

Add `pub mod session;` to `src/lib.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix`
Expected: PASS.

- [ ] **Step 5: Run the full gate and commit**

Run: `mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log`

```bash
git add crates/balerix-plugin-matrix
git commit -m "Cache the matrix session sealed, and decide once whether to log in"
```

---
### Task 12: `plugin.rs`, the SDK surface

The `Plugin` impl does two things: it validates config so a bad agent block still fails the operator's `up`, and it enqueues. It never touches Matrix. Starting the actor is a `Launcher`, so this file stays testable without `matrix-sdk`.

**Files:**
- Create: `crates/balerix-plugin-matrix/src/plugin.rs`
- Modify: `crates/balerix-plugin-matrix/src/lib.rs`

**Interfaces:**
- Consumes: `actor::{Command, Counters, Health, Queue}`, `config::{DaemonConfig, parse_agent, parse_daemon}`.
- Produces: `Launcher` with `launch(&self, config: DaemonConfig, queue: Arc<Queue>) -> Result<(), String>`, async. `MatrixPlugin<L: Launcher>` with `new(metrics: Metrics, counters: Counters, health: Health, queue: Arc<Queue>, launcher: L) -> Self` and `queue(&self) -> Arc<Queue>`.

- [ ] **Step 1: Write the failing tests**

Create `crates/balerix-plugin-matrix/src/plugin.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use balerix_plugin_sdk::testing::{FakeHost, Harness, event};
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeLauncher {
        launched: Arc<Mutex<Vec<DaemonConfig>>>,
        reject: Option<String>,
    }

    impl Launcher for FakeLauncher {
        async fn launch(&self, config: DaemonConfig, _queue: Arc<Queue>) -> Result<(), String> {
            self.launched
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(config);
            match &self.reject {
                Some(m) => Err(m.clone()),
                None => Ok(()),
            }
        }
    }

    fn daemon_json() -> serde_json::Value {
        json!({
            "homeserver": "https://h",
            "userId": "@balerix:h",
            "password": "pw"
        })
    }

    fn plugin(launcher: FakeLauncher) -> (MatrixPlugin<FakeLauncher>, Arc<Queue>, Health) {
        let metrics = Metrics::new("matrix");
        let counters = Counters::new(&metrics).unwrap();
        let health = Health::new();
        let queue = Queue::new(counters.events_dropped.clone());
        let p = MatrixPlugin::new(
            metrics,
            counters,
            health.clone(),
            queue.clone(),
            launcher,
        );
        (p, queue, health)
    }

    #[tokio::test]
    async fn configure_launches_once_and_queues_the_config() {
        let launched = Arc::new(Mutex::new(Vec::new()));
        let (p, queue, _health) = plugin(FakeLauncher {
            launched: launched.clone(),
            reject: None,
        });
        p.configure(daemon_json()).await.unwrap();
        assert_eq!(launched.lock().unwrap().len(), 1);
        assert!(matches!(queue.pop().await, Command::Configure(_)));
    }

    #[tokio::test]
    async fn a_bad_daemon_config_or_a_failed_launch_rejects_configure() {
        let (p, queue, _health) = plugin(FakeLauncher::default());
        let err = p.configure(json!({ "userId": "@a:b" })).await.unwrap_err();
        assert!(err.starts_with("homeserver: "), "{err}");
        assert!(queue.is_empty(), "nothing is queued on a bad config");

        let (p, queue, _health) = plugin(FakeLauncher {
            launched: Arc::new(Mutex::new(Vec::new())),
            reject: Some("whoami: 401".into()),
        });
        assert_eq!(
            p.configure(daemon_json()).await.unwrap_err(),
            "whoami: 401"
        );
        assert!(queue.is_empty(), "nothing is queued on a failed launch");
    }

    #[tokio::test]
    async fn activate_validates_and_enqueues_and_a_bad_block_is_rejected() {
        let (p, queue, _health) = plugin(FakeLauncher::default());
        p.activate("f/c/alice", json!({ "events": ["Stop"] }))
            .await
            .unwrap();
        match queue.pop().await {
            Command::Activate { agent, config } => {
                assert_eq!(agent, "f/c/alice");
                assert!(config.wants("Stop"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            p.activate("f/c/alice", json!({ "events": ["Nope"] }))
                .await
                .unwrap_err(),
            "events[0]: unknown event \"Nope\""
        );
        assert!(queue.is_empty(), "a rejected activate queues nothing");
    }

    #[tokio::test]
    async fn observe_and_deactivate_enqueue_and_health_follows_the_cell() {
        let (p, queue, health) = plugin(FakeLauncher::default());
        p.observe(vec![event("f/c/alice", "Stop", json!({}))]).await;
        assert!(matches!(queue.pop().await, Command::Events(e) if e.len() == 1));
        p.deactivate("f/c/alice").await;
        assert!(matches!(queue.pop().await, Command::Deactivate { .. }));

        assert_eq!(p.health().await, Ok(()));
        health.fail("create room for f/c: no rights".into());
        assert_eq!(
            p.health().await,
            Err("create room for f/c: no rights".into())
        );
    }

    /// The whole surface over the real §4.2 wire format.
    #[tokio::test]
    async fn the_wire_surface_works_end_to_end() {
        let fake = FakeHost::start("tok", daemon_json(), Vec::new()).await;
        let env = fake.env("matrix", std::path::Path::new("scratch"));
        let (p, queue, _health) = plugin(FakeLauncher::default());
        let h = Harness::start(&env, p).await;

        assert!(matches!(queue.pop().await, Command::Configure(_)), "hello configured it");
        h.activate("f/c/alice", json!({})).await.unwrap();
        assert!(matches!(queue.pop().await, Command::Activate { .. }));
        assert_eq!(
            h.activate("f/c/alice", json!({ "nope": 1 })).await.unwrap_err(),
            "nope: unknown field `nope`"
        );
        h.observe(vec![event("f/c/alice", "Stop", json!({}))]).await;
        assert!(matches!(queue.pop().await, Command::Events(_)));
        assert!(h.health().await.is_ok());
        assert!(
            h.metrics().await.contains("balerix_plugin_matrix_"),
            "the registry is served"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix -E 'test(plugin)'`
Expected: FAIL to compile, `cannot find trait 'Launcher'`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/balerix-plugin-matrix/src/plugin.rs`:

```rust
//! The SDK surface (Spec G §6). Everything here validates and enqueues;
//! nothing here talks to Matrix. Starting the actor is a `Launcher`, so
//! this file is testable without a Matrix client.

use std::future::Future;
use std::sync::Arc;

use balerix_api::HookEvent;
use balerix_plugin_sdk::{Metrics, Plugin};
use serde_json::Value;

use crate::actor::{Command, Counters, Health, Queue};
use crate::config::{DaemonConfig, parse_agent, parse_daemon};

/// Proves the credentials and starts the actor and the inbound pump.
/// `Err(message)` fails `configure`, so `serve` returns and the process
/// exits 1 with the message in the plugin's log (Spec G-14).
pub trait Launcher: Send + Sync + 'static {
    fn launch(
        &self,
        config: DaemonConfig,
        queue: Arc<Queue>,
    ) -> impl Future<Output = Result<(), String>> + Send;
}

pub struct MatrixPlugin<L: Launcher> {
    metrics: Metrics,
    #[allow(dead_code, reason = "held so the families outlive the registry")]
    counters: Counters,
    health: Health,
    queue: Arc<Queue>,
    launcher: L,
}

impl<L: Launcher> std::fmt::Debug for MatrixPlugin<L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatrixPlugin")
            .field("queued", &self.queue.len())
            .finish()
    }
}

impl<L: Launcher> MatrixPlugin<L> {
    pub fn new(
        metrics: Metrics,
        counters: Counters,
        health: Health,
        queue: Arc<Queue>,
        launcher: L,
    ) -> Self {
        Self {
            metrics,
            counters,
            health,
            queue,
            launcher,
        }
    }

    pub fn queue(&self) -> Arc<Queue> {
        self.queue.clone()
    }
}

impl<L: Launcher> Plugin for MatrixPlugin<L> {
    /// Parse, prove the credentials, start the actor, then hand it the
    /// config. Nothing is queued unless all three succeed.
    async fn configure(&self, config: Value) -> Result<(), String> {
        let config = parse_daemon(&config).map_err(|e| e.to_string())?;
        self.launcher
            .launch(config.clone(), self.queue.clone())
            .await?;
        self.queue.push(Command::Configure(config));
        Ok(())
    }

    /// Validated here rather than in the actor, so a bad block fails the
    /// operator's `up` instead of failing silently later.
    async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
        let config = parse_agent(&config).map_err(|e| e.to_string())?;
        self.queue.push(Command::Activate {
            agent: agent.to_string(),
            config,
        });
        Ok(())
    }

    async fn deactivate(&self, agent: &str) {
        self.queue.push(Command::Deactivate {
            agent: agent.to_string(),
        });
    }

    /// Enqueue and return: this is a daemon-to-plugin call and must never
    /// wait on a homeserver (Spec G-11).
    async fn observe(&self, events: Vec<HookEvent>) {
        self.queue.push(Command::Events(events));
    }

    async fn health(&self) -> Result<(), String> {
        self.health.get()
    }

    fn metrics(&self) -> Option<&Metrics> {
        Some(&self.metrics)
    }
}
```

Add `pub mod plugin;` to `src/lib.rs`, and re-export the two names a binary needs:

```rust
pub use plugin::{Launcher, MatrixPlugin};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix`
Expected: PASS. If `#[allow(dead_code, reason = ...)]` is rejected by the toolchain, drop the `reason` and keep the comment above the field.

- [ ] **Step 5: Run the full gate and commit**

Run: `mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log`

```bash
git add crates/balerix-plugin-matrix
git commit -m "Validate config and enqueue: the matrix plugin's SDK surface"
```

---

### Task 13: The `matrix-sdk` adapter

The one task whose code cannot be unit-tested, so it is kept as small as the design allows: everything above it is already covered. It is also the one task that adds a large dependency, so `cargo deny` runs here rather than at the end.

**The code below is a sketch against `matrix-sdk 0.18.0`'s documented surface, not a transcription of it.** Step 1 is to read the API and reconcile. Writing it any other way would put invented method names in a plan that claims to have none.

**Files:**
- Create: `crates/balerix-plugin-matrix/src/client.rs`
- Modify: `crates/balerix-plugin-matrix/src/lib.rs`
- Modify: `crates/balerix-plugin-matrix/Cargo.toml`
- Modify: `Cargo.toml` (workspace dependencies)
- Modify: `deny.toml` if the new tree needs it

**Interfaces:**
- Consumes: `matrix::{Inbound, MatrixError, MatrixPort}`, `session::{Plan, Session, plan, load, store, clear, REVOKED}`, `plugin::Launcher`, `actor::{Actor, Command, Counters, Health, Queue}`.
- Produces: `MatrixClient` implementing `MatrixPort`, and `MatrixLauncher { host: Host, counters: Counters, health: Health }` implementing `Launcher`.

- [ ] **Step 1: Read the API before writing any of it**

Open these and note the exact names and signatures:
- `https://docs.rs/matrix-sdk/0.18.0/matrix_sdk/struct.Client.html` — the builder, `homeserver_url`, `sqlite_store`, `matrix_auth`, `whoami`, `sync`, `sync_once`, `add_event_handler`, `create_room`.
- `https://docs.rs/matrix-sdk/0.18.0/matrix_sdk/authentication/matrix/struct.MatrixAuth.html` — `login_username`, `.device_id(..)`, `.initial_device_display_name(..)`, `.request_refresh_token()`, `restore_session`, and the session type it takes.
- `https://docs.rs/matrix-sdk/0.18.0/matrix_sdk/room/struct.Room.html` — `send`, and how a thread relation is attached to `RoomMessageEventContent`.
- `https://docs.rs/matrix-sdk/0.18.0/matrix_sdk/config/struct.SyncSettings.html` — `token`, `timeout`, and the filter that limits an initial sync to zero timeline events.

Write down, in the commit message, any place where the sketch below had to change. Those notes are what a later reader needs.

- [ ] **Step 2: Add the dependency**

In the workspace `Cargo.toml`, under `[workspace.dependencies]`:

```toml
# The Matrix client for balerix-plugin-matrix (Spec G-9). Default features
# carry e2e-encryption, sqlite and automatic-room-key-forwarding; `markdown`
# gives the body plus formatted_body pair the plugin sends, and
# `bundled-sqlite` removes the system SQLite requirement at the cost of a C
# compile, which CI already has. Only balerix-plugin-matrix depends on it,
# so nothing else in the workspace inherits the tree.
matrix-sdk = { version = "0.18.0", features = ["markdown", "bundled-sqlite"] }
```

In `crates/balerix-plugin-matrix/Cargo.toml`, add `matrix-sdk = { workspace = true }` to `[dependencies]`.

- [ ] **Step 3: Check the dependency policy before writing code**

Run: `mise x -- cargo deny check advisories bans sources licenses > /tmp/deny.log 2>&1; tail -40 /tmp/deny.log`
Expected: either clean, or a list of licenses and advisories to decide on.

If `deny.toml` needs entries, make that its own commit first:

```bash
git add deny.toml Cargo.toml Cargo.lock
git commit -m "Allow the licenses matrix-sdk brings into the tree"
```

- [ ] **Step 4: Write the adapter**

Create `crates/balerix-plugin-matrix/src/client.rs`. Reconcile every `matrix_sdk` call against Step 1; the structure, the error mapping and the ordering are what this plan fixes.

The file's head and its private helpers, which the launcher below calls:

```rust
//! The `matrix-sdk` adapter (Spec G §12.3): the only file that knows
//! Matrix types. It implements `MatrixPort` for the actor, and `Launcher`
//! for the plugin, which is where login, the store and the inbound pump
//! are started.

use std::path::Path;
use std::sync::Arc;

use balerix_plugin_sdk::Host;
use matrix_sdk::Client;

use crate::actor::{Actor, Command, Counters, Health, Queue};
use crate::config::{DaemonConfig, Secret};
use crate::matrix::{Inbound, MatrixError, MatrixPort};
use crate::plugin::Launcher;
use crate::session::{self, Plan, Session};

pub struct MatrixClient {
    client: Client,
    user_id: String,
}

/// A client on the homeserver in `config`, with its state and crypto store
/// under `store_dir`. Does not authenticate.
async fn build(config: &DaemonConfig, store_dir: &Path) -> Result<Client, MatrixError>;

/// Restores `session` into a fresh client. No network call.
async fn restore(
    config: &DaemonConfig,
    store_dir: &Path,
    session: &Session,
) -> Result<Client, String>;

/// Password login with the configured device id and display name, asking
/// for a refresh token. Returns the client and the session to seal.
async fn login(
    config: &DaemonConfig,
    store_dir: &Path,
    password: &Secret,
) -> Result<(Client, Session), String>;

/// `GET /_matrix/client/v3/account/whoami`: the call that proves the
/// credentials before the daemon is told the plugin is ready.
async fn whoami(client: &Client) -> Result<String, MatrixError>;

/// The sync loop. Runs forever, pushing `Command::Inbound` onto `queue`
/// for every text message in a room the account is in.
async fn start_inbound_pump(client: Client, host: Host, queue: Arc<Queue>);
```

The three `MatrixPort` methods keep exactly the signatures Task 6 fixed. Their bodies are written from the API read in Step 1; what each must do:

- `user_id` returns the stored `user_id` field. No call.
- `create_room` builds a room-creation request with an invite-only preset, the given `name`, the given `invite` list, and encryption enabled, sends it, and returns the new room's id as a `String`.
- `send` builds a text message from `markdown` so the event carries both a plain `body` and an HTML `formatted_body`. When `thread_root` is `Some`, it attaches an `m.thread` relation rooted at that event id together with the in-reply-to fallback. It sends into the room named by `room` and returns the new event id.
- `react` sends an annotation relation to `event_id` in `room` with `key` as its key.

Rules the reconciled code must honour, each of which the plan asserts elsewhere:

1. **Error mapping.** A homeserver `M_LIMIT_EXCEEDED` becomes `MatrixError::RateLimited { retry_after_ms }` with the server's own delay. A 401 or `M_UNKNOWN_TOKEN` becomes `MatrixError::Auth`. Everything else becomes `MatrixError::Other`. Nothing else in the crate knows Matrix error types.
2. **Rooms are private and encrypted.** Creation sets an invite-only preset, the name, the invite list, and encryption. A room the operator pinned is used as it is.
3. **Store location.** The state and crypto store is `Env::scratch`, joined with `store/`. Nothing goes under `home/` and nothing under `/tmp`.
4. **Device pinning.** Login sets the configured device id and display name and requests a refresh token (G-8).
5. **First sync never replays.** When `Session` had no stored sync token, do one `sync_once` with a filter limiting the timeline to zero events, keep the resulting batch token, and start the live sync from it (§9.3). Persist the token alongside the session so a restart resumes.
6. **The inbound pump filters before it enqueues.** Only `m.room.message` events of msgtype text, with a non-empty body, become `Inbound`. Everything else is dropped in the adapter, so the actor never sees a reaction or a state event.
7. **A rotated token is re-sealed.** `matrix-sdk` rotates the access token when it uses the refresh token, which makes the record in KV stale. Subscribe to the client's session-change signal and call `session::store` with the new tokens whenever it fires. Without this, a restart after a rotation logs in from the password again, or fails when the password has been removed.

Then the launcher:

```rust
pub struct MatrixLauncher {
    pub host: Host,
    pub counters: Counters,
    pub health: Health,
    pub scratch: std::path::PathBuf,
}

impl Launcher for MatrixLauncher {
    async fn launch(&self, config: DaemonConfig, queue: Arc<Queue>) -> Result<(), String> {
        let cached = session::load(&self.host)
            .await
            .map_err(|e| format!("kv: {e}"))?;
        let plan = session::plan(cached, &config)?;
        let (client, session) = match plan {
            Plan::Restore(session) => (restore(&config, &self.scratch, &session).await?, *session),
            Plan::Login { password } => {
                let (client, session) = login(&config, &self.scratch, &password).await?;
                session::store(&self.host, &session)
                    .await
                    .map_err(|e| format!("kv: {e}"))?;
                tracing::warn!(
                    "matrix: logged in with the configured password and cached the session; \
                     the password can now be removed from plugins.yaml"
                );
                (client, session)
            }
        };

        // Prove the credentials before the daemon is told we are ready.
        let user_id = whoami(&client).await.map_err(|e| match e {
            MatrixError::Auth(_) if config.password.is_none() => {
                let _ = &self.host;
                session::REVOKED.to_string()
            }
            other => other.to_string(),
        })?;
        debug_assert_eq!(user_id, session.user_id);

        let port = MatrixClient { client, user_id };
        let pump = start_inbound_pump(&port, queue.clone());
        let mut actor = Actor::new(
            self.host.clone(),
            port,
            self.counters.clone(),
            self.health.clone(),
        );
        actor.load().await;
        tokio::spawn(actor.run(queue));
        tokio::spawn(pump);
        Ok(())
    }
}
```

On an `Auth` failure when a password *is* configured, clear the cached session with `session::clear`, log at warn that it happened, and log in again once before giving up. That is the 401 path of spec §5.2 step 4.

- [ ] **Step 5: Verify it compiles and nothing regressed**

Run: `mise x -- cargo clippy -p balerix-plugin-matrix --all-targets -- -D warnings`
Expected: clean. No `todo!` or `unimplemented!` may remain.

Run: `mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log`
Expected: every earlier test still passes; the adapter has none of its own.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock deny.toml crates/balerix-plugin-matrix
git commit -m "Add the matrix-sdk adapter behind the plugin's port"
```

The commit body records every place the real API differed from the sketch.

---
### Task 14: The binary, the package, and the integration test

Wires everything into a process the daemon can start, and proves the whole path over the real wire format with a fake port.

**Files:**
- Modify: `crates/balerix-plugin-matrix/src/main.rs` (replacing the Task 4 placeholder)
- Create: `crates/balerix-plugin-matrix/package/balerix-plugin.yaml`
- Create: `crates/balerix-plugin-matrix/package/mise.toml`
- Create: `crates/balerix-plugin-matrix/tests/plugin_it.rs`
- Modify: `scripts/package-plugins.sh`

**Interfaces:**
- Consumes: everything from Tasks 4 through 13.
- Produces: the `balerix-plugin-matrix` binary and a directory package under `target/plugins/matrix/`.

- [ ] **Step 1: Write the manifest and the package task file**

`crates/balerix-plugin-matrix/package/balerix-plugin.yaml`:

```yaml
apiVersion: balerix/v1
kind: Plugin
name: matrix
version: 0.1.0
protocol: 1
start: serve
# Every event is observed: the set is per-agent config (Spec G-3), so the
# filtering is the plugin's job, not the subscription's.
hooks:
  observe: [SessionStart, SessionEnd, UserPromptSubmit, PreToolUse, PostToolUse, Notification, Stop, SubagentStop, PreCompact]
  intercept: []
# fleets: the phase-change feed. actions: send_text. kv: the maps and the
# sealed session.
needs: [fleets, actions, kv]
routes: false
```

`crates/balerix-plugin-matrix/package/mise.toml`, copying the flow plugin's:

```toml
# The development package layout (plugins spec §17.6): the binary is copied
# into bin/ by `mise run package-plugins`. A release package pins the binary
# as a mise tool instead and carries no binary of its own.
[tools]

[tasks.serve]
run = "./bin/balerix-plugin-matrix"
```

In `scripts/package-plugins.sh`, change the loop to `for name in flow web matrix; do`.

- [ ] **Step 2: Write `main.rs`**

```rust
//! `balerix-plugin-matrix`: read the daemon's environment, build the
//! actor's plumbing, say hello, serve. Failures print `matrix: …` to
//! stderr and exit 1; that lands in the plugin's tmux window and
//! `plugins/matrix/logs/`. The actor itself does not start until
//! `configure` arrives with the homeserver and the credentials.

use balerix_plugin_matrix::actor::{Counters, Health, Queue};
use balerix_plugin_matrix::client::MatrixLauncher;
use balerix_plugin_matrix::MatrixPlugin;
use balerix_plugin_sdk::{Env, Host, Metrics, serve};

fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let env = Env::from_process()?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let host = Host::new(env.clone())?;
        let metrics = Metrics::new(&env.name);
        let counters = Counters::new(&metrics)?;
        let health = Health::new();
        let queue = Queue::new(counters.events_dropped.clone());
        let launcher = MatrixLauncher {
            host: Host::new(env.clone())?,
            counters: counters.clone(),
            health: health.clone(),
            scratch: env.scratch.clone(),
        };
        let plugin = MatrixPlugin::new(metrics, counters, health, queue, launcher);
        eprintln!("matrix: starting");
        serve(&host, env!("CARGO_PKG_VERSION"), plugin).await?;
        Ok(())
    })
}

fn main() {
    if let Err(e) = run() {
        eprintln!("matrix: {e:#}");
        std::process::exit(1);
    }
}
```

Add `pub mod client;` to `src/lib.rs`.

- [ ] **Step 3: Write the failing integration test**

`crates/balerix-plugin-matrix/tests/plugin_it.rs`:

```rust
//! The whole plugin over the real §4.2 wire format, with a fake Matrix
//! port: `Harness` drives it exactly as the daemon would.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use balerix_api::PluginAction;
use balerix_plugin_matrix::actor::{Actor, Command, Counters, Health, Queue};
use balerix_plugin_matrix::config::DaemonConfig;
use balerix_plugin_matrix::matrix::fake::{Call, FakePort};
use balerix_plugin_matrix::matrix::Inbound;
use balerix_plugin_matrix::plugin::Launcher;
use balerix_plugin_matrix::MatrixPlugin;
use balerix_plugin_sdk::testing::{FakeHost, Harness, event};
use balerix_plugin_sdk::{Host, Metrics};
use serde_json::json;

/// Starts a real actor over a fake port, which is what the `matrix-sdk`
/// launcher does over a real one.
struct TestLauncher {
    host: Host,
    port: FakePort,
    counters: Counters,
    health: Health,
}

impl Launcher for TestLauncher {
    async fn launch(&self, _config: DaemonConfig, queue: Arc<Queue>) -> Result<(), String> {
        let mut actor = Actor::new(
            self.host.clone(),
            self.port.clone(),
            self.counters.clone(),
            self.health.clone(),
        );
        actor.load().await;
        tokio::spawn(actor.run(queue));
        Ok(())
    }
}

async fn eventually(label: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {label}");
}

fn sends(calls: &[Call]) -> Vec<(Option<String>, String)> {
    calls
        .iter()
        .filter_map(|c| match c {
            Call::Send {
                thread_root, body, ..
            } => Some((thread_root.clone(), body.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_session_gets_a_thread_and_a_reply_in_it_reaches_the_agent() {
    let config = json!({
        "homeserver": "https://h",
        "userId": "@balerix:h",
        "password": "pw",
        "invite": ["@rahul:h"]
    });
    let fake = FakeHost::start("tok", config, Vec::new()).await;
    let env = fake.env("matrix", std::path::Path::new("scratch"));
    let host = Host::new(env.clone()).unwrap();

    let metrics = Metrics::new("matrix");
    let counters = Counters::new(&metrics).unwrap();
    let health = Health::new();
    let queue = Queue::new(counters.events_dropped.clone());
    let port = FakePort::new("@balerix:h");
    let plugin = MatrixPlugin::new(
        metrics,
        counters.clone(),
        health,
        queue.clone(),
        TestLauncher {
            host,
            port: port.clone(),
            counters,
            health: Health::new(),
        },
    );

    let h = Harness::start(&env, plugin).await;
    h.activate("payments/backend/alice", json!({})).await.unwrap();

    let mut start = event(
        "payments/backend/alice",
        "SessionStart",
        json!({ "source": "startup" }),
    );
    start.session_id = Some("s1".into());
    let mut note = event(
        "payments/backend/alice",
        "Notification",
        json!({ "message": "Claude needs your permission to use Bash" }),
    );
    note.session_id = Some("s1".into());
    h.observe(vec![start, note]).await;

    let p = port.clone();
    eventually("the room and two messages", move || {
        let calls = p.calls();
        calls.iter().any(|c| matches!(c, Call::CreateRoom { .. })) && sends(&calls).len() == 2
    })
    .await;

    let calls = port.calls();
    let s = sends(&calls);
    assert_eq!(s[0].0, None, "the root is not a thread reply");
    assert!(s[1].0.is_some(), "the notification is in the thread");
    assert!(s[1].1.contains("permission to use Bash"), "{}", s[1].1);

    let (room, root) = match (&calls[0], &calls[1]) {
        (Call::CreateRoom { .. }, Call::Send { room, .. }) => (room.clone(), "$evt2:fake".to_string()),
        other => panic!("unexpected calls: {other:?}"),
    };

    queue.push(Command::Inbound(Inbound {
        room,
        event_id: "$reply:fake".into(),
        sender: "@rahul:h".into(),
        thread_root: Some(root),
        body: "run the tests".into(),
    }));

    let f = &fake;
    eventually("the send_text", || {
        !f.actions_for("payments/backend/alice").is_empty()
    })
    .await;
    assert_eq!(
        fake.actions_for("payments/backend/alice"),
        vec![PluginAction::SendText {
            text: "run the tests".into(),
            submit: true,
        }]
    );

    assert!(
        h.metrics().await.contains("balerix_plugin_matrix_inbound_total"),
        "the outcome counter is published"
    );
}
```

- [ ] **Step 4: Run the test to verify it fails, then passes**

Run: `mise x -- cargo nextest run -p balerix-plugin-matrix --test plugin_it`
Expected first: FAIL to compile until `pub mod client;` and the package files exist. Then PASS.

If the root event id `$evt2:fake` does not match, read the recorded calls and use the id the second `Call::Send` actually produced rather than hard-coding a guess.

- [ ] **Step 5: Verify the package assembles**

Run: `mise run package-plugins && ls target/plugins/matrix`
Expected: `bin/`, `balerix-plugin.yaml`, `mise.toml`.

- [ ] **Step 6: Run the full gate and commit**

Run: `mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log`

```bash
git add crates/balerix-plugin-matrix scripts/package-plugins.sh
git commit -m "Wire the matrix plugin into a binary and package it"
```

---

### Task 15: Documentation and the manual check

The accepted risk is the part that must not be left to a commit message, and the manual check is the only thing that exercises a real homeserver.

**Files:**
- Modify: `docs/THREAT-MODEL.md`
- Modify: `AGENTS.md`
- Modify: `docs/plugin-protocol.md` if it documents `plugins.yaml` entries
- Create: `scripts/verify-matrix.sh`
- Modify: `mise.toml`

**Interfaces:**
- Consumes: everything.
- Produces: a `verify-matrix` mise task.

- [ ] **Step 1: Record the accepted risk**

Add to `docs/THREAT-MODEL.md`, in the section that holds the other accepted risks, matching the surrounding format:

> **Matrix room membership is the plugin's access control (Spec G-5).** Anyone in a crew's room can post a thread reply that the matrix plugin turns into a prompt for a sandboxed agent with repository write access and, unless the crew sets `git.push: false`, push rights. A mis-set join rule, a careless invite or a homeserver administrator therefore reaches every agent in that crew. Bounding it: rooms the plugin creates are private, invite-only and encrypted; the invite list is daemon config rather than per-agent; and the agent remains inside its nono profile, so the blast radius is that crew's repository and whatever the agent's own settings permit. An operator wanting a narrower boundary puts one crew per room and invites accordingly. Accepted deliberately; the alternative, an explicit allowlist of Matrix ids, is designed in Spec G §1.1 and not built.

- [ ] **Step 2: Add the gotchas**

Add to the `## Gotchas` list in `AGENTS.md`:

```markdown
- The matrix plugin's `matrix-sdk` state and crypto store lives in the
  plugin's `scratch/`, so `plugin remove --purge` discards the device keys:
  the bot rejoins as a new device and previously encrypted history stops
  being readable by it. Purge only when you mean that.
- `plugins.yaml` entries may carry `secrets: { <config key>: <path> }`. The
  daemon reads each file at load, trims one trailing newline and injects the
  value into the entry's `config` before `hello`. The file must not be
  readable by group or others. The resolved config lives only in memory;
  `ResolvedPlugin` hand-implements `Debug` and prints `config: <redacted>`,
  so never go back to deriving it.
- Rotating a file named in `secrets` changes `ResolvedPlugin::hash` and so
  restarts the plugin on the next sync. That is intended.
- `matrix-sdk` is a large tree and only `balerix-plugin-matrix` depends on
  it. Keep it that way, and run `cargo deny` when bumping it.
- A plugin that needs daemon-level config implements `Plugin::configure`.
  The daemon may deliver an `activate` before `configure` returns, so such a
  plugin must buffer, as the matrix actor does.
```

- [ ] **Step 3: Update the plugins.yaml documentation**

Run: `grep -n "plugins.yaml" -A 12 docs/plugin-protocol.md | head -60`

If the file shows a `plugins.yaml` example, add a `secrets` line to it with a one-sentence explanation matching Step 2. If it does not, skip this step and say so in the commit message.

- [ ] **Step 4: Write the manual check**

`scripts/verify-matrix.sh`, modelled on `scripts/verify-claude.sh`:

```bash
#!/usr/bin/env bash
# Spec G's manual check against a real homeserver. Not part of any CI tier:
# it needs credentials and a server, so it is run by hand before a release.
#
# Required environment:
#   MATRIX_HOMESERVER  https://matrix.example.org
#   MATRIX_USER_ID     @balerix-test:example.org
#   MATRIX_PASSWORD    the bot account's password
#   MATRIX_INVITE      @you:example.org
#
# What it does: starts a daemon under target/tmp/verify-matrix with the
# matrix plugin loaded, brings up a one-crew fleet with two agents, and
# prints what to look for. Everything it writes is under target/tmp.
set -euo pipefail
cd "$(dirname "$0")/.."

for var in MATRIX_HOMESERVER MATRIX_USER_ID MATRIX_PASSWORD MATRIX_INVITE; do
  if [[ -z "${!var:-}" ]]; then
    echo "verify-matrix: $var is not set" >&2
    exit 1
  fi
done

root="$PWD/target/tmp/verify-matrix"
rm -rf "$root"
mkdir -p "$root/config/balerix" "$root/secrets"
printf '%s' "$MATRIX_PASSWORD" > "$root/secrets/matrix-password"
chmod 600 "$root/secrets/matrix-password"

mise run package-plugins

cat > "$root/config/balerix/plugins.yaml" <<YAML
plugins:
  - name: matrix
    source: $PWD/target/plugins/matrix
    secrets:
      password: $root/secrets/matrix-password
    config:
      homeserver: $MATRIX_HOMESERVER
      userId: $MATRIX_USER_ID
      invite: ["$MATRIX_INVITE"]
YAML

cat <<'NOTES'
verify-matrix: config written. Now, by hand:

  1. Start the daemon with XDG_CONFIG_HOME, XDG_STATE_HOME, XDG_DATA_HOME
     and HOME pointed under target/tmp/verify-matrix, as `mise run serve`
     does, and confirm `balerix plugin list` shows matrix ready.
  2. `balerix up` a fleet with one crew and two agents, both with an empty
     `plugins: { matrix: {} }` block.

Then check, in your Matrix client:

  - exactly one room appeared, named "balerix <fleet>/<crew>", private and
    encrypted, with you invited;
  - each agent has its own thread, rooted on a session-started message;
  - a permission prompt from an agent appears in that agent's thread;
  - replying inside a live thread reaches the agent and the reply is
    acknowledged with a reaction;
  - a message posted at room level is refused with a reaction;
  - after SessionEnd, a reply in that thread is refused with a reaction;
  - restarting the daemon replays nothing into the room.

Finally, confirm the password can be removed: delete the `secrets` block,
restart, and check the plugin still logs in from its cached session.
NOTES
```

Run: `chmod +x scripts/verify-matrix.sh`

Add to `mise.toml`:

```toml
[tasks.verify-matrix]
description = "Spec G's manual check against a real Matrix homeserver (needs MATRIX_* credentials; not part of any CI tier)"
run = "scripts/verify-matrix.sh"
```

- [ ] **Step 5: Verify the script's own failure path**

Run: `env -u MATRIX_HOMESERVER mise run verify-matrix; echo "exit=$?"`
Expected: `verify-matrix: MATRIX_HOMESERVER is not set` and a non-zero exit.

- [ ] **Step 6: Run the full gate and commit**

Run: `mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log`

```bash
git add AGENTS.md docs mise.toml scripts/verify-matrix.sh
git commit -m "Document the matrix plugin's accepted risk, gotchas and manual check"
```

---

## Done when

Every box above is ticked, and:

1. `mise run check` passes and `mise x -- cargo deny check` passes.
2. `mise run package-plugins` produces `target/plugins/matrix/`.
3. `Plugin::configure` exists with its default, and flow and web are unchanged.
4. `PluginEntry.secrets` resolves at load, errors on collision, missing file and loose permissions, and no rendering path emits a value.
5. `scripts/verify-matrix.sh` has been run against a real homeserver and every bullet in its checklist observed.
6. `docs/THREAT-MODEL.md` carries the accepted risk from spec §11.
