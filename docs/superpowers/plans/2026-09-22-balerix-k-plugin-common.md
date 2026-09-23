# Spec K: `balerix-plugin-common` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract the channel-independent half of the matrix plugin and the web plugin's review message into a new published library crate, `balerix-plugin-common`, add the pure `answer` decision core, and make the crate its own release unit, with matrix and web behaving exactly as before.

**Architecture:** `plugins/common/` is a standalone Cargo project (not a workspace member) that depends on `balerix-api` and `balerix-plugin-sdk` by `{ version, path }` so the same manifest publishes to crates.io. Modules move from matrix (`render`, `question`, `pending`, `config` helpers, the actor's `Queue`/`Health`/counters, `phase_changes`) and from web (`review`) with their tests and snapshots; one new module, `answer`, turns Spec J §7.2/§7.3 into a pure decision function that matrix's actor executes. The release scripts learn a "library" unit kind: versioned and published like the core crates, with no binary, image or package.

**Tech Stack:** Rust 1.98 (edition 2024), cargo-nextest, insta, proptest, prometheus via the SDK's `Metrics`, bash release scripts under `scripts/release/`, mise tasks.

**Spec:** `docs/superpowers/specs/2026-09-22-balerix-k-plugin-common-design.md`

## Global Constraints

- Run cargo through mise: `mise x -- cargo …`, or `mise run <task>`. Plugin projects are checked with `scripts/plugin.sh check <name>` (`mise run plugin <name>`), never with `cargo --workspace`.
- Every dependency version is exact and matches the core workspace's pin where the crate appears there (`Cargo.toml` `[workspace.dependencies]`): serde 1.0.229, serde_json 1.0.151, serde_path_to_error 0.1.20, thiserror 2.0.20, tokio 1.53.1, tracing 0.1.44, prometheus 0.14.0, insta 1.48.0, proptest 1.11.0.
- New crates and dependencies are stated in the commit message that adds them (AGENTS.md).
- `unsafe_code = "forbid"`; clippy `all` warn, `unwrap_used`/`expect_used` warn (allowed in tests via `clippy.toml`).
- Behaviour of matrix and web does not change: `mise run plugin matrix` and `mise run plugin web` must pass with no `.snap` content edits (K-3). Moved snapshot files are renamed only.
- Library crates return `thiserror` errors whose messages start with the config path.
- Never hand-edit a version in `Cargo.toml`; `plugins/common` starts at `0.1.0`, the version the SDK is released at (`balerix-v0.1.0` exists).
- insta: read a `.snap.new`, compare against expectation, then `mise x -- cargo insta accept`; never blind-accept.
- Commit after every task; PR titles are Conventional Commits and the final PR title for this plan is `feat(common): extract balerix-plugin-common from matrix and web (Spec K)`.

---

## File Structure

**Created**

- `plugins/common/Cargo.toml`, `Cargo.lock`, `clippy.toml`, `deny.toml`, `README.md` — the standalone published project.
- `plugins/common/src/lib.rs` — module list and re-exports.
- `plugins/common/src/config.rs` — `Secret`, `ConfigError`, `deserialize`, `EventFilter`, `validate_key_delay` (from matrix `config.rs`).
- `plugins/common/src/queue.rs` — `Queue<C>`, `Health` (from matrix `actor.rs`).
- `plugins/common/src/metrics.rs` — `Shared` (the counter families every chat plugin registers).
- `plugins/common/src/render.rs` + `src/snapshots/` — from matrix `render.rs`; `split` takes the limit.
- `plugins/common/src/question.rs` — from matrix, verbatim; `fixtures` always compiled.
- `plugins/common/src/pending.rs` — from matrix, verbatim.
- `plugins/common/src/phases.rs` — `PhaseChange`, `phase_changes`, `run` (from matrix `render.rs`/`plugin.rs`/`main.rs`).
- `plugins/common/src/review.rs` — from web `review.rs`; `ReviewBody` renamed `Review`.
- `plugins/common/src/answer.rs` — new: `on_reply`, `on_closed`, `Decision`, `Verdict`, `Reaction`.

**Modified**

- `plugins/matrix/Cargo.toml`, `src/lib.rs`, `src/config.rs`, `src/actor.rs`, `src/plugin.rs`, `src/main.rs`, `src/client.rs`, `src/render.rs` (deleted), `src/question.rs` (deleted), `src/pending.rs` (deleted), `src/snapshots/` (moved), `examples/question_plan.rs`, `tests/plugin_it.rs`.
- `plugins/web/Cargo.toml`, `src/lib.rs`, `src/review.rs`, `src/routes.rs`.
- `scripts/plugin.sh` (no manifest for a library), `mise.toml` (tasks), `.github/workflows/ci.yml` (plugin matrix).
- `scripts/release/lib.sh`, `prepare.sh`, `plan.sh`, `publish-crates.sh`, `affected-units.sh`, `test.sh`; `.github/workflows/release.yml`, `release-pr.yml`; `docs/RELEASING.md`.
- `ARCHITECTURE.md`, `AGENTS.md`.

---

### Task 1: Scaffold the `plugins/common` project and wire it into the tooling

**Files:**
- Create: `plugins/common/Cargo.toml`, `plugins/common/src/lib.rs`, `plugins/common/clippy.toml`, `plugins/common/deny.toml`, `plugins/common/README.md`
- Modify: `scripts/plugin.sh:22-31`, `mise.toml` (`fmt`, `plugins`, `audit`, `lint` tasks), `.github/workflows/ci.yml:56-57`

**Interfaces:**
- Produces: the crate `balerix-plugin-common` at `plugins/common`, checked by `mise run plugin common`.

- [ ] **Step 1: Write the manifest**

`plugins/common/Cargo.toml`:

```toml
# A standalone project, not a workspace member (Spec H, Spec K-6): a
# plugin's dependency tree stays out of the daemon's feature resolution.
# Published to crates.io (Spec K-5), so the two core dependencies carry a
# version beside their path; scripts/release/prepare.sh moves it with the
# core release.
[workspace]
resolver = "3"

[package]
name = "balerix-plugin-common"
description = "Shared building blocks for balerix chat plugins: event rendering, the AskUserQuestion answer flow, config helpers, a drop-oldest queue and phase diffing (Spec K)"
version = "0.1.0"
edition = "2024"
rust-version = "1.98"
license = "Apache-2.0"
repository = "https://github.com/balerix-ai/balerix"
readme = "README.md"
publish = true

[lib]
path = "src/lib.rs"

[dependencies]
balerix-api = { version = "0.1.0", path = "../../crates/balerix-api" }
balerix-plugin-sdk = { version = "0.1.0", path = "../../crates/balerix-plugin-sdk" }
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
serde_path_to_error = "0.1.20"
thiserror = "2.0.20"
tokio = { version = "1.53.1", features = ["rt-multi-thread", "macros", "sync", "time"] }
tracing = "0.1.44"

[dev-dependencies]
insta = { version = "1.48.0", features = ["yaml", "json"] }
# The model-based test of the key plan (Spec J §10) moves here with question.rs.
proptest = "1.11.0"

[lints.rust]
unsafe_code = "forbid"

[lints.clippy]
all = { level = "warn", priority = -1 }
unwrap_used = "warn"
expect_used = "warn"
```

`plugins/common/src/lib.rs`:

```rust
//! `balerix-plugin-common` (Spec K): the channel-independent half of a
//! balerix chat plugin. Rendering, the `AskUserQuestion` answer flow,
//! config helpers, a drop-oldest command queue, shared metric families
//! and phase diffing — everything the matrix plugin needed that knew
//! nothing about Matrix. See README.md for how to build a plugin on it.
```

`plugins/common/clippy.toml`:

```toml
allow-unwrap-in-tests = true
allow-expect-in-tests = true
```

`plugins/common/deny.toml` (web's policy; this tree is smaller):

```toml
[advisories]
ignore = []

[licenses]
# The allowance list is deliberately broader than this project's tree, so a
# legitimate new dependency does not fail the audit tier; don't warn about
# the entries nothing here happens to use.
unused-allowed-license = "allow"
allow = [
  "MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause", "BSD-3-Clause",
  "ISC", "Zlib", "Unicode-3.0", "Unicode-DFS-2016", "MPL-2.0", "CC0-1.0", "0BSD",
]

[bans]
multiple-versions = "warn"
wildcards = "deny"

[sources]
unknown-registry = "deny"
unknown-git = "deny"
```

`plugins/common/README.md` (the walkthrough is Task 13; a stub now so `readme` resolves):

```markdown
# balerix-plugin-common

Shared building blocks for balerix chat plugins (Spec K). Walkthrough to follow.
```

- [ ] **Step 2: Generate the lockfile and confirm it builds**

Run: `cd plugins/common && mise x -- cargo generate-lockfile && mise x -- cargo build -q`
Expected: a `Cargo.lock` appears; the build succeeds with an empty library.

- [ ] **Step 3: Teach `scripts/plugin.sh` that a library has no package manifest**

Edit `version_check` in `scripts/plugin.sh`:

```bash
version_check() {
  local cargo_version manifest_version
  # A library project (plugins/common) ships no package; nothing to compare.
  [[ -f $dir/package/balerix-plugin.yaml ]] || return 0
  cargo_version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$dir/Cargo.toml" | head -n 1)
  manifest_version=$(sed -n 's/^version: //p' "$dir/package/balerix-plugin.yaml")
  if [[ $cargo_version != "$manifest_version" ]]; then
    echo "plugins/$name: Cargo.toml says $cargo_version, package/balerix-plugin.yaml says $manifest_version" >&2
    exit 1
  fi
}
```

- [ ] **Step 4: Add the project to the mise tasks**

In `mise.toml`:

- `[tasks.fmt]` `run` gains `"scripts/plugin.sh fmt common",` before the flow line.
- `[tasks.plugins]` `run` gains `"scripts/plugin.sh check common",` before the flow line, and its description becomes `"Lint and test every standalone plugin project, the shared library included (its own tier; not part of `check`)"`.
- `[tasks.audit]` `run` gains, before the flow lines:
  ```
  "cargo audit --file plugins/common/Cargo.lock",
  "cargo deny --manifest-path plugins/common/Cargo.toml check advisories bans sources licenses",
  ```
- `[tasks.lint]` `run` gains, after the `cargo package … -p balerix-plugin-sdk` line:
  ```
  "cargo package --no-verify --allow-dirty --manifest-path plugins/common/Cargo.toml",
  ```
- `[tasks.plugin]` `usage` help text becomes `help="the plugin directory under plugins/ (common is the shared library)"`.

- [ ] **Step 5: Add the project to CI**

In `.github/workflows/ci.yml`, the `plugins` job's matrix becomes:

```yaml
        plugin: [common, flow, web, matrix]
```

- [ ] **Step 6: Run the plugin check and the lint task**

Run: `mise run plugin common && mise run lint`
Expected: both pass (an empty crate with zero tests; `cargo package` for common succeeds).

- [ ] **Step 7: Commit**

```bash
git add plugins/common scripts/plugin.sh mise.toml .github/workflows/ci.yml
git commit -m "feat(common): scaffold balerix-plugin-common as a standalone published project (Spec K §3)

New crate. Dependencies: balerix-api and balerix-plugin-sdk by version and
path (published crate), serde, serde_json, serde_path_to_error, thiserror,
tokio, tracing; dev: insta, proptest. All at the core workspace's pins."
```

---

### Task 2: `config` — `Secret`, `ConfigError`, `deserialize`, `EventFilter`, `validate_key_delay`

**Files:**
- Create: `plugins/common/src/config.rs`
- Modify: `plugins/common/src/lib.rs`, `plugins/matrix/Cargo.toml`, `plugins/matrix/src/config.rs`, `plugins/matrix/src/actor.rs`, `plugins/matrix/src/plugin.rs`

**Interfaces:**
- Produces:
  - `pub struct Secret(String)` with `new`, `expose`, redacted `Debug`, `#[serde(transparent)]`.
  - `pub struct ConfigError { pub path: String, pub message: String }` (`thiserror`, Display `path: message`, message alone when path empty).
  - `pub fn deserialize<T: DeserializeOwned>(config: &Value) -> Result<T, ConfigError>`.
  - `pub const DEFAULT_EVENTS: [&str; 4]`, `pub const LIFECYCLE: [&str; 2]`.
  - `pub struct EventFilter(pub Vec<String>)` (`#[serde(transparent)]`, `Default` = the curated four) with `pub fn wants(&self, event: &str) -> bool` and `pub fn validate(&self) -> Result<(), ConfigError>`.
  - `pub fn validate_key_delay(ms: u64) -> Result<(), ConfigError>`.

- [ ] **Step 1: Write the failing tests in common**

Create `plugins/common/src/config.rs` with only the tests module and the doc header, then add `pub mod config;` to `lib.rs`:

```rust
//! Config helpers every plugin re-implemented (Spec K §3.1): a redacted
//! `Secret`, a path-first `ConfigError`, the deserializer that attaches
//! serde's path, and the curated event filter of Spec G §4.2.

use std::fmt;

use balerix_api::{HOOK_EVENTS, MAX_KEY_DELAY_MS, MIN_KEY_DELAY_MS};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_secret_never_prints_its_value() {
        let s = Secret::new("hunter2");
        assert_eq!(s.expose(), "hunter2");
        let text = format!("{s:?}");
        assert!(!text.contains("hunter2") && text.contains("<redacted>"), "{text}");
        assert_eq!(serde_json::to_value(&s).unwrap(), json!("hunter2"), "transparent on the wire");
    }

    #[test]
    fn a_config_error_prints_its_path_first_or_the_message_alone() {
        let e = ConfigError { path: "homeserver".into(), message: "missing".into() };
        assert_eq!(e.to_string(), "homeserver: missing");
        let e = ConfigError { path: String::new(), message: "not a map".into() };
        assert_eq!(e.to_string(), "not a map");
    }

    #[derive(Debug, serde::Deserialize, PartialEq)]
    #[serde(deny_unknown_fields, default)]
    struct Sample {
        a: u32,
        b: String,
    }
    impl Default for Sample {
        fn default() -> Self {
            Self { a: 1, b: "x".into() }
        }
    }

    #[test]
    fn deserialize_rejects_a_non_map_and_attaches_the_path() {
        assert_eq!(
            deserialize::<Sample>(&json!({})).unwrap(),
            Sample { a: 1, b: "x".into() }
        );
        let e = deserialize::<Sample>(&json!([])).unwrap_err();
        assert_eq!(e.path, "");
        assert_eq!(e.message, "invalid type: array, expected a map");
        let e = deserialize::<Sample>(&json!({ "nope": 1 })).unwrap_err();
        assert_eq!(e.to_string(), "nope: unknown field `nope`");
        let e = deserialize::<Sample>(&json!({ "a": "no" })).unwrap_err();
        assert_eq!(e.path, "a");
    }

    #[derive(Debug, serde::Deserialize)]
    struct Needs {
        #[allow(dead_code)]
        homeserver: String,
    }

    #[test]
    fn a_missing_required_field_names_itself_in_the_path() {
        let e = deserialize::<Needs>(&json!({})).unwrap_err();
        assert_eq!(e.path, "homeserver");
        assert_eq!(e.to_string(), "homeserver: missing field `homeserver`");
    }

    #[test]
    fn the_event_filter_defaults_to_the_curated_set_and_always_wants_lifecycle() {
        let f = EventFilter::default();
        assert_eq!(f.0, DEFAULT_EVENTS.map(String::from).to_vec());
        assert!(f.wants("Notification"));
        assert!(!f.wants("PreToolUse"));
        let f = EventFilter(vec!["PreToolUse".into()]);
        assert!(f.wants("PreToolUse"));
        assert!(!f.wants("Notification"), "the list replaces the default");
        assert!(f.wants("SessionStart") && f.wants("SessionEnd"), "lifecycle is always wanted");
        assert_eq!(f.validate(), Ok(()));
    }

    #[test]
    fn an_unknown_event_is_refused_with_its_index() {
        let f = EventFilter(vec!["Stop".into(), "Frobnicate".into()]);
        assert_eq!(
            f.validate().unwrap_err().to_string(),
            "events[1]: unknown event \"Frobnicate\""
        );
    }

    #[test]
    fn the_key_delay_is_bounded() {
        assert_eq!(validate_key_delay(100), Ok(()));
        for bad in [19, 501] {
            let e = validate_key_delay(bad).unwrap_err();
            assert_eq!(e.path, "keyDelayMs");
            assert!(e.message.contains("20 to 500"), "{e}");
        }
    }
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cd plugins/common && mise x -- cargo nextest run --config-file ../../.config/nextest.toml`
Expected: compile errors — `Secret`, `ConfigError`, `deserialize`, `EventFilter`, `validate_key_delay` not found.

- [ ] **Step 3: Move the implementation from matrix**

Above the tests in `plugins/common/src/config.rs`, add — copied from `plugins/matrix/src/config.rs` — `Secret` (with its `impl Secret` and `impl fmt::Debug`), `ConfigError` (struct and `Display`), and `deserialize`, each verbatim except that `deserialize` becomes `pub`. Then add the filter:

```rust
/// The curated default event set (Spec G §4.2).
pub const DEFAULT_EVENTS: [&str; 4] = ["SessionStart", "Notification", "Stop", "SessionEnd"];
/// Always wanted: these open and close a conversation, so a filter cannot
/// suppress them.
pub const LIFECYCLE: [&str; 2] = ["SessionStart", "SessionEnd"];

/// Which hook events a plugin shows (Spec G §4.2): the list replaces the
/// default, and the lifecycle pair is wanted whatever the list says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventFilter(pub Vec<String>);

impl Default for EventFilter {
    fn default() -> Self {
        Self(DEFAULT_EVENTS.iter().map(|e| (*e).to_string()).collect())
    }
}

impl EventFilter {
    pub fn wants(&self, event: &str) -> bool {
        LIFECYCLE.contains(&event) || self.0.iter().any(|e| e == event)
    }

    /// Every name must be one of `HOOK_EVENTS`; the error names the
    /// offending index under `events`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (i, name) in self.0.iter().enumerate() {
            if !HOOK_EVENTS.contains(&name.as_str()) {
                return Err(ConfigError {
                    path: format!("events[{i}]"),
                    message: format!("unknown event {name:?}"),
                });
            }
        }
        Ok(())
    }
}

/// Spec J §7.5: the pause between keys, `20..=500` ms, reported under
/// the camelCase key every plugin block uses.
pub fn validate_key_delay(ms: u64) -> Result<(), ConfigError> {
    if !(MIN_KEY_DELAY_MS..=MAX_KEY_DELAY_MS).contains(&ms) {
        return Err(ConfigError {
            path: "keyDelayMs".into(),
            message: format!("expected {MIN_KEY_DELAY_MS} to {MAX_KEY_DELAY_MS} milliseconds, got {ms}"),
        });
    }
    Ok(())
}
```

- [ ] **Step 4: Run the common tests**

Run: `cd plugins/common && mise x -- cargo nextest run --config-file ../../.config/nextest.toml`
Expected: 8 tests pass.

- [ ] **Step 5: Point matrix at common**

`plugins/matrix/Cargo.toml` `[dependencies]` gains, after the SDK line:

```toml
balerix-plugin-common = { path = "../common" }
```

Rewrite `plugins/matrix/src/config.rs`: delete `Secret`, `ConfigError`, `deserialize`, `DEFAULT_EVENTS`, `LIFECYCLE`, the `wants` method and the two validation loops; replace with:

```rust
use balerix_api::DEFAULT_KEY_DELAY_MS;
pub use balerix_plugin_common::config::{
    ConfigError, DEFAULT_EVENTS, EventFilter, LIFECYCLE, Secret, deserialize, validate_key_delay,
};
```

`AgentConfig.events` becomes `pub events: EventFilter`, its `Default` uses `EventFilter::default()`, and:

```rust
impl AgentConfig {
    /// Lifecycle events are always posted; everything else is filtered by
    /// `events` (Spec G §4.2).
    pub fn wants(&self, event: &str) -> bool {
        self.events.wants(event)
    }
}

pub fn parse_agent(config: &Value) -> Result<AgentConfig, ConfigError> {
    let c: AgentConfig = deserialize(config)?;
    c.events.validate()?;
    validate_key_delay(c.key_delay_ms)?;
    Ok(c)
}
```

`parse_daemon` keeps its three checks and calls `deserialize`. In the remaining matrix config tests, delete the ones that moved (`a_missing_required_field_names_itself_in_the_path` stays, it tests `parse_daemon`; the secret-in-debug assertion stays inside `daemon_config_defaults_the_device_and_keeps_the_secret_out_of_debug`), and change `assert_eq!(c.events, DEFAULT_EVENTS.map(String::from).to_vec());` to `assert_eq!(c.events.0, DEFAULT_EVENTS.map(String::from).to_vec());`. `actor.rs` and `plugin.rs` compile unchanged (they call `config.wants`).

- [ ] **Step 6: Run the matrix check**

Run: `mise run plugin matrix`
Expected: fmt, clippy and every test pass; no snapshot changes.

- [ ] **Step 7: Commit**

```bash
git add plugins/common plugins/matrix
git commit -m "refactor(common): move Secret, ConfigError, deserialize and the event filter out of matrix (Spec K §3.1)"
```

---

### Task 3: `queue` — `Queue<C>` and `Health`

**Files:**
- Create: `plugins/common/src/queue.rs`
- Modify: `plugins/common/src/lib.rs`, `plugins/matrix/src/actor.rs`

**Interfaces:**
- Produces: `pub struct Queue<C>` with `pub fn new(dropped: IntCounter) -> Arc<Self>`, `push(&self, C)`, `async fn pop(&self) -> C`, `len`, `is_empty`; `pub const QUEUE: usize = OBSERVER_QUEUE`; `pub struct Health` with `new`, `ok`, `fail(String)`, `get() -> Result<(), String>`.
- Matrix keeps `pub type Queue = balerix_plugin_common::queue::Queue<Command>;` and `pub use balerix_plugin_common::queue::Health;` in `actor.rs`, so `plugin_it.rs`, `client.rs`, `main.rs` and `plugin.rs` compile unchanged.

- [ ] **Step 1: Write the failing tests**

`plugins/common/src/queue.rs`:

```rust
//! The bounded drop-oldest command queue every observer plugin feeds its
//! actor from (Spec G-11), and the health cell the actor writes and
//! `Plugin::health` reads.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use balerix_api::OBSERVER_QUEUE;
use balerix_plugin_sdk::metrics::IntCounter;
use tokio::sync::Notify;

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_plugin_sdk::Metrics;

    fn dropped() -> IntCounter {
        Metrics::new("t").int_counter("events_dropped_total", "t").unwrap()
    }

    #[tokio::test]
    async fn the_queue_is_fifo_and_wakes_a_waiting_pop() {
        let q: Arc<Queue<u32>> = Queue::new(dropped());
        assert!(q.is_empty());
        let waiter = {
            let q = q.clone();
            tokio::spawn(async move { q.pop().await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        q.push(1);
        q.push(2);
        assert_eq!(waiter.await.unwrap(), 1);
        assert_eq!(q.pop().await, 2);
    }

    #[tokio::test]
    async fn a_full_queue_drops_the_oldest_and_counts_it() {
        let counter = dropped();
        let q: Arc<Queue<usize>> = Queue::new(counter.clone());
        for i in 0..=QUEUE {
            q.push(i);
        }
        assert_eq!(q.len(), QUEUE);
        assert_eq!(counter.get(), 1);
        assert_eq!(q.pop().await, 1, "entry 0 was dropped");
    }

    #[test]
    fn health_starts_ok_and_reports_the_last_failure_until_cleared() {
        let h = Health::new();
        assert_eq!(h.get(), Ok(()));
        h.fail("a".into());
        h.fail("b".into());
        assert_eq!(h.get(), Err("b".into()));
        h.ok();
        assert_eq!(h.get(), Ok(()));
    }
}
```

Add `pub mod queue;` to `lib.rs`.

- [ ] **Step 2: Run to see the failure**

Run: `cd plugins/common && mise x -- cargo nextest run --config-file ../../.config/nextest.toml queue`
Expected: compile error, `Queue`/`Health` not found.

- [ ] **Step 3: Move `Queue` and `Health` from matrix, generic over the command**

Above the tests, copied from `plugins/matrix/src/actor.rs` with `Command` replaced by the type parameter:

```rust
/// Queue depth, the same as the daemon's own observer queues.
pub const QUEUE: usize = OBSERVER_QUEUE;

/// A bounded queue that drops its oldest entry rather than blocking its
/// producer: `observe` is a daemon-to-plugin HTTP call and must return.
pub struct Queue<C> {
    inner: Mutex<VecDeque<C>>,
    notify: Notify,
    dropped: IntCounter,
}

impl<C> Queue<C> {
    pub fn new(dropped: IntCounter) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(VecDeque::with_capacity(QUEUE)),
            notify: Notify::new(),
            dropped,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<C>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn push(&self, command: C) {
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

    pub async fn pop(&self) -> C {
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

- [ ] **Step 4: Run the common tests**

Run: `cd plugins/common && mise x -- cargo nextest run --config-file ../../.config/nextest.toml`
Expected: all pass.

- [ ] **Step 5: Rewire matrix**

In `plugins/matrix/src/actor.rs`: delete `QUEUE`, `struct Queue`, its `impl`, `struct Health` and its `impl`, and the three tests `the_queue_is_fifo_and_wakes_a_waiting_pop`, `a_full_queue_drops_the_oldest_and_counts_it`, `health_starts_ok_and_reports_the_last_failure_until_cleared` (they moved). Add, after the `use` block:

```rust
pub use balerix_plugin_common::queue::{Health, QUEUE};
/// The actor's inbound queue over this plugin's commands.
pub type Queue = balerix_plugin_common::queue::Queue<Command>;
```

Remove the now-unused imports (`VecDeque`, `Notify`, `OBSERVER_QUEUE`).

- [ ] **Step 6: Run the matrix check**

Run: `mise run plugin matrix`
Expected: pass.

- [ ] **Step 7: Commit**

```bash
git add plugins/common plugins/matrix
git commit -m "refactor(common): move the drop-oldest Queue and Health out of matrix, generic over the command (Spec K §3.1)"
```

---

### Task 4: `metrics::Shared`

**Files:**
- Create: `plugins/common/src/metrics.rs`
- Modify: `plugins/common/src/lib.rs`, `plugins/matrix/src/actor.rs`

**Interfaces:**
- Produces: `pub struct Shared { pub events_dropped: IntCounter, pub messages_sent: IntCounterVec, pub inbound: IntCounterVec, pub errors: IntCounterVec, pub answers_mismatched: IntCounter }` with `pub fn new(metrics: &Metrics) -> Result<Self, SdkError>`.
- Matrix's `Counters` keeps its five public fields plus `rooms` and `threads_open`; it is built from a `Shared` so nothing that reads `counters.inbound` changes.

- [ ] **Step 1: Write the failing test**

`plugins/common/src/metrics.rs`:

```rust
//! The metric families every chat plugin registers (Spec G §10, Spec J
//! §8), through the SDK's prefixing registry so the names come out as
//! `balerix_plugin_<name>_…`.

use balerix_plugin_sdk::metrics::{IntCounter, IntCounterVec};
use balerix_plugin_sdk::{Metrics, SdkError};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_carries_the_plugin_prefix() {
        let metrics = Metrics::new("chat");
        let s = Shared::new(&metrics).unwrap();
        s.events_dropped.inc();
        s.messages_sent.with_label_values(&["event"]).inc();
        s.inbound.with_label_values(&["routed"]).inc();
        s.errors.with_label_values(&["send"]).inc();
        s.answers_mismatched.inc();
        let text = metrics.render().unwrap();
        for family in [
            "events_dropped_total",
            "messages_sent_total",
            "inbound_total",
            "errors_total",
            "answers_mismatched_total",
        ] {
            assert!(text.contains(&format!("balerix_plugin_chat_{family}")), "{family}: {text}");
        }
    }
}
```

Add `pub mod metrics;` to `lib.rs`.

- [ ] **Step 2: Run to see the failure**

Run: `cd plugins/common && mise x -- cargo nextest run --config-file ../../.config/nextest.toml metrics`
Expected: compile error, `Shared` not found.

- [ ] **Step 3: Implement**

```rust
/// The counters shared by every chat plugin. Label values are the
/// plugin's own; Spec G §10 and Spec J §8 name the conventional ones.
#[derive(Debug, Clone)]
pub struct Shared {
    /// `events_dropped_total`: commands dropped because the queue was full.
    pub events_dropped: IntCounter,
    /// `messages_sent_total{kind}`: messages posted to the channel.
    pub messages_sent: IntCounterVec,
    /// `inbound_total{outcome}`: channel messages seen, by what became of them.
    pub inbound: IntCounterVec,
    /// `errors_total{kind}`: channel and daemon failures.
    pub errors: IntCounterVec,
    /// `answers_mismatched_total`: answers Claude recorded differently from
    /// what the channel chose (Spec J-6).
    pub answers_mismatched: IntCounter,
}

impl Shared {
    pub fn new(metrics: &Metrics) -> Result<Self, SdkError> {
        Ok(Self {
            events_dropped: metrics.int_counter(
                "events_dropped_total",
                "Commands dropped because the queue was full",
            )?,
            messages_sent: metrics.int_counter_vec(
                "messages_sent_total",
                "Messages sent to the channel, by kind",
                &["kind"],
            )?,
            inbound: metrics.int_counter_vec(
                "inbound_total",
                "Channel messages seen, by what became of them",
                &["outcome"],
            )?,
            errors: metrics.int_counter_vec("errors_total", "Failures, by kind", &["kind"])?,
            answers_mismatched: metrics.int_counter(
                "answers_mismatched_total",
                "Answers Claude recorded differently from what was chosen",
            )?,
        })
    }
}
```

- [ ] **Step 4: Run the common tests**

Run: `cd plugins/common && mise x -- cargo nextest run --config-file ../../.config/nextest.toml`
Expected: pass.

- [ ] **Step 5: Rewire matrix's `Counters`**

In `plugins/matrix/src/actor.rs` replace `Counters` and its `new`:

```rust
/// The metric families of Spec G §10: the shared set plus the two gauges
/// only a room-and-thread plugin has.
#[derive(Debug, Clone)]
pub struct Counters {
    pub messages_sent: IntCounterVec,
    pub events_dropped: IntCounter,
    pub inbound: IntCounterVec,
    pub rooms: IntGauge,
    pub threads_open: IntGauge,
    pub errors: IntCounterVec,
    pub answers_mismatched: IntCounter,
}

impl Counters {
    pub fn new(metrics: &Metrics) -> Result<Self, SdkError> {
        let shared = Shared::new(metrics)?;
        Ok(Self {
            messages_sent: shared.messages_sent,
            events_dropped: shared.events_dropped,
            inbound: shared.inbound,
            rooms: metrics.int_gauge("rooms", "Crew rooms the plugin knows")?,
            threads_open: metrics.int_gauge("threads_open", "Agent sessions with an open thread")?,
            errors: shared.errors,
            answers_mismatched: shared.answers_mismatched,
        })
    }
}
```

with `use balerix_plugin_common::metrics::Shared;`. The help strings for the shared families change wording (`Messages sent to Matrix, by kind` → `…to the channel…`); nothing asserts on them (`every_metric_family_carries_the_plugin_prefix` checks names only).

- [ ] **Step 6: Run the matrix check**

Run: `mise run plugin matrix`
Expected: pass.

- [ ] **Step 7: Commit**

```bash
git add plugins/common plugins/matrix
git commit -m "refactor(common): register the shared metric families through metrics::Shared (Spec K §3.2)"
```

---

### Task 5: `render` with a parameterised `split`, and its snapshots

**Files:**
- Create: `plugins/common/src/render.rs`, `plugins/common/src/snapshots/*.snap` (moved)
- Modify: `plugins/common/src/lib.rs`, `plugins/matrix/src/lib.rs`, `plugins/matrix/src/render.rs` (deleted), `plugins/matrix/src/actor.rs`, `plugins/matrix/src/config.rs`

**Interfaces:**
- Produces: `render::{short_session, thread_root, event_message, question_message, phase_message, split}` with `pub fn split(text: &str, limit: usize, max_parts: usize) -> Vec<String>`; `PhaseChange` re-exported from `phases` (Task 8 creates `phases.rs`; until then `PhaseChange` lives in `render.rs` and Task 8 moves it).
- Consumes: `question::Question` — `question.rs` moves in Task 6, so this task moves `question.rs` and `pending.rs` *first* (Step 3) since `render` depends on `question` and `pending` on both. The three move in one task to keep the crate compiling at every commit.

- [ ] **Step 1: Move `question.rs` verbatim, with fixtures always compiled**

`git mv plugins/matrix/src/question.rs plugins/common/src/question.rs`. In the moved file, change `#[cfg(test)] pub(crate) mod fixtures {` to:

```rust
/// Dialogs the tests share. Always compiled, like `FakePort` and the
/// SDK's `testing`: downstream crates build their tests on them.
pub mod fixtures {
```

Replace every `crate::question::` in its tests with `super::` where needed (the tests already `use super::fixtures::*` and `use crate::question::{…}`; change the latter to `use super::{…}`). Add `pub mod question;` to `lib.rs`.

- [ ] **Step 2: Move `pending.rs` verbatim**

`git mv plugins/matrix/src/pending.rs plugins/common/src/pending.rs`. Change `tracing::warn!("matrix: …")` to `tracing::warn!("…")` (three occurrences: `bad question record`, `mirroring the question for`, `clearing the question for`). Add `pub mod pending;` to `lib.rs`. Its `use crate::question::…` paths stay valid inside common.

- [ ] **Step 3: Move `render.rs` and its snapshots; parameterise `split`**

`git mv plugins/matrix/src/render.rs plugins/common/src/render.rs` and

```bash
mkdir -p plugins/common/src/snapshots
for f in plugins/matrix/src/snapshots/balerix_plugin_matrix__render__tests__*.snap; do
  git mv "$f" "plugins/common/src/snapshots/$(basename "$f" | sed 's/^balerix_plugin_matrix__/balerix_plugin_common__/')"
done
```

In the moved `render.rs`:

- Replace the `BODY_LIMIT` constant and its doc with:
  ```rust
  /// Room left in every part for its `(n/N)` marker and, inside a code
  /// block, the fence this chunker closes and reopens around the break.
  /// A `limit` passed to `split` must exceed it.
  pub const PART_OVERHEAD: usize = 64;
  ```
  (delete the later private `PART_OVERHEAD`).
- Change `split`'s signature and first lines to:
  ```rust
  /// `text` as the messages to post, in order. One part when it fits
  /// `limit`; otherwise chunks that each fit it, every part marked
  /// `(n/N)`. Past `max_parts` the last part says how many were dropped —
  /// the only case that loses text. Matrix passes 4000 (a phone's
  /// screen); GitHub passes 65 536 (a comment).
  pub fn split(text: &str, limit: usize, max_parts: usize) -> Vec<String> {
      if text.len() <= limit {
          return vec![text.to_string()];
      }
      let max_parts = max_parts.max(1);
      let mut parts = chunks(text, limit.saturating_sub(PART_OVERHEAD).max(1));
  ```
- In its tests, add `const BODY_LIMIT: usize = 4000;` at the top of `mod tests` and change every `split(x, n)` call to `split(x, BODY_LIMIT, n)`.
- `use crate::question::Question;` stays valid.

Add `pub mod render;` to `lib.rs`.

- [ ] **Step 4: Run the common tests**

Run: `cd plugins/common && mise x -- cargo nextest run --config-file ../../.config/nextest.toml`
Expected: every moved test passes, including the five snapshot tests against the renamed files (insta matches by module path `balerix_plugin_common__render__tests__…`). If a `.snap.new` appears, the rename was wrong; fix the name, do not accept.

- [ ] **Step 5: Rewire matrix**

- `plugins/matrix/src/lib.rs`: remove `pub mod pending; pub mod question; pub mod render;` and add
  ```rust
  pub use balerix_plugin_common::{pending, question, render};
  ```
- `plugins/matrix/src/config.rs`: add `pub const BODY_LIMIT: usize = 4000;` with the doc from the old `render.rs` ("One message holds this much (Spec G §8)…").
- `plugins/matrix/src/actor.rs`: `render::split(body, max_parts)` becomes `render::split(body, crate::config::BODY_LIMIT, max_parts)`; the two test references `crate::render::BODY_LIMIT` become `crate::config::BODY_LIMIT`. `use crate::question;`, `use crate::render::{self, PhaseChange};`, `use crate::pending::…` keep working through the re-exports.
- `plugins/matrix/examples/question_plan.rs`: `use balerix_plugin_matrix::question::…` keeps working through the re-export; leave it.

- [ ] **Step 6: Run the matrix check**

Run: `mise run plugin matrix`
Expected: pass, no snapshot changes (`git status` shows no `.snap.new`).

- [ ] **Step 7: Commit**

```bash
git add plugins/common plugins/matrix
git commit -m "refactor(common): move render, question and pending out of matrix; split takes its limit (Spec K-4)"
```

---

### Task 6: `phases` — `PhaseChange`, `phase_changes`, `run`

**Files:**
- Create: `plugins/common/src/phases.rs`
- Modify: `plugins/common/src/render.rs`, `plugins/common/src/lib.rs`, `plugins/matrix/src/plugin.rs`, `plugins/matrix/src/main.rs`, `plugins/matrix/src/actor.rs`

**Interfaces:**
- Produces: `phases::PhaseChange { agent, from: AgentPhase, to: AgentPhase, message }`, `pub fn phase_changes(previous: Option<&[FleetRecord]>, current: &[FleetRecord]) -> Vec<PhaseChange>`, `pub async fn run(host: Host, sink: impl FnMut(Vec<PhaseChange>) + Send) -> Infallible`. `render::PhaseChange` is a re-export.

- [ ] **Step 1: Write the failing test for `run`**

`plugins/common/src/phases.rs`:

```rust
//! Agent phase changes from `fleets/watch` (Spec G §8's phase feed). The
//! watch yields whole snapshots, so a *change* is derived by diffing the
//! new frame against the previous one, keyed by each agent's full id.

use std::collections::BTreeMap;
use std::convert::Infallible;

use balerix_api::{AgentPhase, AgentStatus, FleetRecord};
use balerix_plugin_sdk::Host;

/// One agent's phase transition, from the fleet watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseChange {
    pub agent: String,
    pub from: AgentPhase,
    pub to: AgentPhase,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::FleetSpec;
    use balerix_plugin_sdk::testing::FakeHost;
    use std::sync::{Arc, Mutex};

    /// One `fleets/watch` frame: a single fleet carrying exactly the
    /// agents given, each at its phase and status message.
    fn fleet(agents: &[(&str, AgentPhase, &str)]) -> Vec<FleetRecord> {
        let mut record = FleetRecord::new(FleetSpec { name: "f".into(), ..Default::default() });
        for (id, phase, message) in agents {
            record.status.agents.insert(
                (*id).to_string(),
                AgentStatus { phase: *phase, message: (*message).to_string(), ..Default::default() },
            );
        }
        vec![record]
    }

    #[test]
    fn the_first_frame_seeds_silently_and_reports_nothing() {
        let current = fleet(&[("f/c/alice", AgentPhase::Ready, "")]);
        assert_eq!(phase_changes(None, &current), Vec::new());
    }

    #[test]
    fn an_agent_that_changed_phase_is_reported() {
        let previous = fleet(&[("f/c/alice", AgentPhase::Starting, "")]);
        let current = fleet(&[("f/c/alice", AgentPhase::Ready, "agent ready")]);
        assert_eq!(
            phase_changes(Some(&previous), &current),
            vec![PhaseChange {
                agent: "f/c/alice".into(),
                from: AgentPhase::Starting,
                to: AgentPhase::Ready,
                message: "agent ready".into(),
            }]
        );
    }

    #[test]
    fn an_agent_whose_phase_held_is_not_reported() {
        let previous = fleet(&[("f/c/alice", AgentPhase::Ready, "")]);
        let current = fleet(&[("f/c/alice", AgentPhase::Ready, "")]);
        assert_eq!(phase_changes(Some(&previous), &current), Vec::new());
    }

    #[test]
    fn an_agent_that_appeared_or_disappeared_is_not_reported() {
        let none = fleet(&[]);
        let one = fleet(&[("f/c/alice", AgentPhase::Pending, "")]);
        assert_eq!(phase_changes(Some(&none), &one), Vec::new());
        assert_eq!(phase_changes(Some(&one), &none), Vec::new());
    }

    #[tokio::test]
    async fn run_feeds_the_sink_with_changes_between_frames() {
        let fake = FakeHost::start("tok", serde_json::json!({}), fleet(&[("f/c/a", AgentPhase::Starting, "")])).await;
        let host = Host::new(fake.env("t", std::path::Path::new("scratch"))).unwrap();
        let seen: Arc<Mutex<Vec<PhaseChange>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let seen = seen.clone();
            move |changes: Vec<PhaseChange>| seen.lock().unwrap().extend(changes)
        };
        let task = tokio::spawn(run(host, sink));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        fake.set_fleets(fleet(&[("f/c/a", AgentPhase::Ready, "up")]));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while seen.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        task.abort();
        let got = seen.lock().unwrap().clone();
        assert_eq!(got.len(), 1, "the first frame seeds; the second reports: {got:?}");
        assert_eq!(got[0].to, AgentPhase::Ready);
    }
}
```

Check `FakeHost`'s API for the third `start` argument and the setter that pushes a new watch frame: `grep -n 'pub fn\|pub async fn' crates/balerix-plugin-sdk/src/testing.rs`. If the setter is named differently from `set_fleets`, use the real name; if none exists, the web plugin's `state` tests show how frames are pushed — follow them.

- [ ] **Step 2: Run to see the failure**

Run: `cd plugins/common && mise x -- cargo nextest run --config-file ../../.config/nextest.toml phases`
Expected: compile error, `phase_changes`/`run` not found.

- [ ] **Step 3: Implement**

Move `phase_changes` and `flatten` verbatim from `plugins/matrix/src/plugin.rs` into `phases.rs` (delete their tests there: they are the four above). Add:

```rust
/// The `fleets/watch` loop: keeps the previous frame, hands every
/// non-empty diff to `sink`, and never returns — `FleetWatch::next`
/// reconnects on its own. Spawn it and abort the task at exit.
pub async fn run(host: Host, mut sink: impl FnMut(Vec<PhaseChange>) + Send) -> Infallible {
    let mut watch = host.watch_fleets();
    let mut previous: Option<Vec<FleetRecord>> = None;
    loop {
        let current = watch.next().await;
        let changes = phase_changes(previous.as_deref(), &current);
        if !changes.is_empty() {
            sink(changes);
        }
        previous = Some(current);
    }
}
```

In `render.rs`, delete `struct PhaseChange` and add `pub use crate::phases::PhaseChange;`. Add `pub mod phases;` to `lib.rs`.

- [ ] **Step 4: Run the common tests**

Run: `cd plugins/common && mise x -- cargo nextest run --config-file ../../.config/nextest.toml`
Expected: pass.

- [ ] **Step 5: Rewire matrix**

- `plugins/matrix/src/plugin.rs`: delete `phase_changes`, `flatten`, the `phase_changes_tests` module and the now-unused imports (`BTreeMap`, `AgentStatus`, `FleetRecord`); keep `use crate::render::PhaseChange;` (still valid through the re-export).
- `plugins/matrix/src/main.rs`: delete `watch_phases` and its imports (`FleetRecord`, `phase_changes`); replace the spawn with
  ```rust
  let queue = plugin.queue();
  let phase_watch = tokio::spawn(balerix_plugin_common::phases::run(
      host.clone(),
      move |changes| queue.push(Command::Phases(changes)),
  ));
  ```
- `plugins/matrix/src/lib.rs`: add `phases` to the re-export line: `pub use balerix_plugin_common::{pending, phases, question, render};`.

- [ ] **Step 6: Run the matrix check**

Run: `mise run plugin matrix`
Expected: pass.

- [ ] **Step 7: Commit**

```bash
git add plugins/common plugins/matrix
git commit -m "refactor(common): move phase diffing and the fleets/watch loop into phases (Spec K §3.3)"
```

---

### Task 7: `review` — from the web plugin

**Files:**
- Create: `plugins/common/src/review.rs`
- Modify: `plugins/common/src/lib.rs`, `plugins/web/Cargo.toml`, `plugins/web/src/lib.rs`, `plugins/web/src/review.rs`, `plugins/web/src/routes.rs`

**Interfaces:**
- Produces: `review::{Side, Comment, Review, MAX_COMMENTS, MAX_BODY_BYTES, MAX_TEXT_BYTES, MAX_REF_BYTES, MAX_MESSAGE_BYTES, validate, render_message}` with `Review { head, base_ref, summary, comments }` (the former `ReviewBody`).
- Web keeps `pub type ReviewBody = balerix_plugin_common::review::Review;` and its `pub use` list, so its routes and tests compile unchanged.

- [ ] **Step 1: Move the module**

`git mv plugins/web/src/review.rs plugins/common/src/review.rs`. In the moved file: rename `ReviewBody` to `Review` everywhere (struct, docs, tests), add `pub` to nothing new (everything used is already `pub`), and change the module doc's first line to `//! A code review as one message for the agent (Spec C §4.4, Spec K-7): the body a review page or a GitHub review is mapped to, its limits, and the message \`send_text\` delivers.` Add `pub mod review;` to `lib.rs`.

- [ ] **Step 2: Run the common tests**

Run: `cd plugins/common && mise x -- cargo nextest run --config-file ../../.config/nextest.toml review`
Expected: the four moved tests pass.

- [ ] **Step 3: Rewire web**

- `plugins/web/Cargo.toml` `[dependencies]` gains `balerix-plugin-common = { path = "../common" }`.
- Create a new `plugins/web/src/review.rs`:
  ```rust
  //! The review submission (Spec C §4.4). The types, limits and renderer
  //! live in `balerix_plugin_common::review` (Spec K-7); this keeps the
  //! route's name for the request body.

  pub use balerix_plugin_common::review::{
      Comment, MAX_BODY_BYTES, MAX_COMMENTS, MAX_MESSAGE_BYTES, MAX_REF_BYTES, MAX_TEXT_BYTES,
      Side, render_message, validate,
  };

  /// What `POST /agents/{id}/review` takes.
  pub type ReviewBody = balerix_plugin_common::review::Review;
  ```
- `plugins/web/src/lib.rs` and `routes.rs` need no change (`crate::review::{MAX_MESSAGE_BYTES, ReviewBody, render_message, validate}` resolves).

- [ ] **Step 4: Run the web check**

Run: `mise run plugin web`
Expected: pass (web's `review` tests moved; its route tests still exercise the type through the alias).

- [ ] **Step 5: Commit**

```bash
git add plugins/common plugins/web
git commit -m "refactor(common): move the review message out of web as review::Review (Spec K-7)"
```

---

### Task 8: `answer` — the pure decision core

**Files:**
- Create: `plugins/common/src/answer.rs`
- Modify: `plugins/common/src/lib.rs`, `plugins/common/src/pending.rs` (one method)

**Interfaces:**
- Produces:
  ```rust
  pub enum Reaction { Ack, Refused, Failed, Confirmed }
  pub struct Decision {
      pub post: Option<String>,
      pub send: Option<PluginAction>,
      pub stage: Stage,
      pub react: Option<Reaction>,
      pub outcome: Option<&'static str>,
  }
  impl Decision { pub fn gates_on_post(&self) -> bool }
  pub fn on_reply(open: &OpenQuestion, reply: &str, key_delay_ms: u64) -> Decision;
  pub enum Verdict { Confirmed { echo: Option<String> }, Mismatch { message: String }, AnsweredAtTerminal { message: String }, Nothing }
  pub fn on_closed(open: &OpenQuestion, answers: &Value) -> Verdict;
  ```
  and `Stage::with_echo(self, echo: Option<String>) -> Stage` in `pending.rs`.
- Texts are matrix's exact strings (Spec K §4): `**answering** {chosen}`, `**I read that as** {chosen}. Reply **yes** to send.`, `**declining the question**`, `an answer is already on its way; wait for the agent.`, `this answer needs more keystrokes than can be sent from here ({reason}); answer at the terminal.`, `**recorded answer differs** — Claude recorded {recorded}; you chose {chosen}. Tell the agent if that matters.`, `**answered at the terminal** {recorded}`.

- [ ] **Step 1: Write the failing tests**

`plugins/common/src/answer.rs`:

```rust
//! The answer flow of Spec J §7.2 and §7.3 as pure decisions (Spec K §4).
//! No I/O and no port: a plugin's actor executes a `Decision` through its
//! own channel, under the contract in `Decision`'s docs.

use balerix_api::{KeyStep, PluginAction};
use serde_json::Value;

use crate::pending::{OpenQuestion, Stage};
use crate::question::{self, Matched, Refusal, Selection};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::question::fixtures::{color, colors_multi, input, size};
    use balerix_api::Key;
    use serde_json::json;

    fn open(questions: &[Value]) -> OpenQuestion {
        OpenQuestion {
            questions: question::parse(&input(questions)).unwrap(),
            stage: Stage::Open,
            posted: true,
            notified: false,
        }
    }

    fn keys(d: &Decision) -> Vec<KeyStep> {
        match &d.send {
            Some(PluginAction::SendKeys { steps, .. }) => steps.clone(),
            other => panic!("expected send_keys, got {other:?}"),
        }
    }

    #[test]
    fn an_exact_reply_echoes_and_plans_the_keys() {
        let d = on_reply(&open(&[color()]), "3", 100);
        assert_eq!(d.post.as_deref(), Some("**answering** Color → Blue"));
        assert_eq!(keys(&d), vec![KeyStep::Key(Key::Down), KeyStep::Key(Key::Down), KeyStep::Key(Key::Enter)]);
        assert!(matches!(d.send, Some(PluginAction::SendKeys { delay_ms: 100, .. })));
        assert!(matches!(d.stage, Stage::Sent { selections: Some(_), echo: None }));
        assert_eq!(d.react, Some(Reaction::Ack));
        assert_eq!(d.outcome, Some("answered"));
        assert!(d.gates_on_post());
    }

    #[test]
    fn the_agents_key_delay_is_used() {
        let d = on_reply(&open(&[color()]), "1", 250);
        assert!(matches!(d.send, Some(PluginAction::SendKeys { delay_ms: 250, .. })));
    }

    #[test]
    fn an_inexact_reply_asks_first() {
        let d = on_reply(&open(&[color()]), "gre", 100);
        assert_eq!(d.post.as_deref(), Some("**I read that as** Color → Green. Reply **yes** to send."));
        assert!(d.send.is_none());
        assert!(matches!(d.stage, Stage::Confirming { echo: None, .. }));
        assert_eq!(d.react, None);
        assert_eq!(d.outcome, Some("confirm_asked"));
        assert!(d.gates_on_post());
    }

    #[test]
    fn yes_sends_the_held_selection_without_a_second_echo() {
        let mut o = open(&[color()]);
        let asked = on_reply(&o, "gre", 100);
        o.stage = asked.stage.with_echo(Some("$echo".into()));
        let d = on_reply(&o, "YES", 100);
        assert!(d.post.is_none());
        assert_eq!(keys(&d), vec![KeyStep::Key(Key::Down), KeyStep::Key(Key::Enter)]);
        assert!(matches!(&d.stage, Stage::Sent { echo: Some(e), .. } if e == "$echo"));
        assert_eq!(d.react, Some(Reaction::Ack));
        assert_eq!(d.outcome, Some("confirmed"));
    }

    #[test]
    fn no_drops_the_confirmation_and_another_reply_is_matched_fresh() {
        let mut o = open(&[color()]);
        o.stage = on_reply(&o, "gre", 100).stage;
        let d = on_reply(&o, "n", 100);
        assert!(d.post.is_none() && d.send.is_none());
        assert_eq!(d.stage, Stage::Open);
        assert_eq!(d.react, Some(Reaction::Ack));
        assert_eq!(d.outcome, None);
        let d = on_reply(&o, "2", 100);
        assert_eq!(d.post.as_deref(), Some("**answering** Color → Green"));
    }

    #[test]
    fn prose_is_refused_with_the_option_list() {
        let d = on_reply(&open(&[color()]), "purple please", 100);
        let post = d.post.unwrap();
        assert!(post.contains("Red") && post.contains("Blue"), "{post}");
        assert!(d.send.is_none());
        assert_eq!(d.stage, Stage::Open);
        assert_eq!(d.react, Some(Reaction::Refused));
        assert_eq!(d.outcome, Some("answer_refused"));
        assert!(!d.gates_on_post(), "a refusal that fails to post still refuses");
    }

    #[test]
    fn skip_declines_with_one_escape() {
        let d = on_reply(&open(&[color()]), "skip", 100);
        assert_eq!(d.post.as_deref(), Some("**declining the question**"));
        assert_eq!(keys(&d), vec![KeyStep::Key(Key::Escape)]);
        assert!(matches!(d.stage, Stage::Sent { selections: None, echo: None }));
        assert_eq!(d.outcome, Some("skipped"));
    }

    #[test]
    fn a_reply_while_keys_are_on_their_way_is_refused_and_the_stage_kept() {
        let mut o = open(&[color()]);
        o.stage = Stage::Sent { selections: None, echo: Some("$e".into()) };
        let d = on_reply(&o, "2", 100);
        assert_eq!(d.post.as_deref(), Some("an answer is already on its way; wait for the agent."));
        assert!(d.send.is_none());
        assert_eq!(d.stage, o.stage);
        assert_eq!(d.react, Some(Reaction::Refused));
        assert_eq!(d.outcome, Some("answer_refused"));
    }

    #[test]
    fn one_reply_answers_several_questions() {
        let d = on_reply(&open(&[colors_multi(), size()]), "1, 3\n2", 100);
        assert_eq!(d.post.as_deref(), Some("**answering** Colors → Red, Blue · Size → Medium"));
        assert_eq!(d.outcome, Some("answered"));
    }

    #[test]
    fn a_plan_too_long_for_the_delay_is_refused_before_anything_is_sent() {
        let options: Vec<Value> = (1..=70).map(|i| json!({ "label": format!("o{i}"), "description": "" })).collect();
        let q = json!({ "question": "Which?", "header": "H", "multiSelect": false, "options": options });
        let d = on_reply(&open(&[q]), "70", 500);
        assert!(d.post.unwrap().starts_with("this answer needs more keystrokes than can be sent from here ("));
        assert!(d.send.is_none());
        assert_eq!(d.stage, Stage::Open);
        assert_eq!(d.react, Some(Reaction::Refused));
        assert_eq!(d.outcome, Some("answer_refused"));
    }

    #[test]
    fn a_sent_answer_is_confirmed_or_reported_when_it_differs() {
        let mut o = open(&[color()]);
        o.stage = on_reply(&o, "3", 100).stage.with_echo(Some("$echo".into()));
        assert_eq!(
            on_closed(&o, &json!({ "Which color?": "Blue" })),
            Verdict::Confirmed { echo: Some("$echo".into()) }
        );
        assert_eq!(
            on_closed(&o, &json!({ "Which color?": "Red" })),
            Verdict::Mismatch {
                message: "**recorded answer differs** — Claude recorded Color → Red; you chose Color → Blue. Tell the agent if that matters.".into()
            }
        );
    }

    #[test]
    fn an_answer_given_at_the_terminal_is_reported_and_a_skip_says_nothing() {
        let o = open(&[color()]);
        assert_eq!(
            on_closed(&o, &json!({ "Which color?": "Green" })),
            Verdict::AnsweredAtTerminal { message: "**answered at the terminal** Color → Green".into() }
        );
        let mut skipped = open(&[color()]);
        skipped.stage = Stage::Sent { selections: None, echo: None };
        assert_eq!(on_closed(&skipped, &json!({})), Verdict::Nothing);
    }
}
```

Add `pub mod answer;` to `lib.rs`. The exact `describe` outputs (`Color → Blue`, `Colors → Red, Blue · Size → Medium`) come from `question::describe`; confirm against `plugins/common/src/question.rs` tests before relying on them and adjust the expected strings to what `describe` produces.

- [ ] **Step 2: Run to see the failure**

Run: `cd plugins/common && mise x -- cargo nextest run --config-file ../../.config/nextest.toml answer`
Expected: compile error, `on_reply` not found.

- [ ] **Step 3: Add `Stage::with_echo` to `pending.rs`**

```rust
impl Stage {
    /// The same stage carrying the echo message's id, once it is known.
    /// `Open` has no echo and is returned unchanged.
    pub fn with_echo(self, echo: Option<String>) -> Stage {
        match self {
            Stage::Open => Stage::Open,
            Stage::Confirming { selections, .. } => Stage::Confirming { selections, echo },
            Stage::Sent { selections, .. } => Stage::Sent { selections, echo },
        }
    }
}
```

- [ ] **Step 4: Implement `answer.rs`**

```rust
/// The reaction on the operator's message once a decision is executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaction {
    Ack,
    Refused,
    Failed,
    Confirmed,
}

/// What to do with one reply while a question is open (Spec J §7.2).
///
/// **Executor contract** (every plugin honours it, Spec K §4): post `post`
/// first. If it did not land and `gates_on_post()` is true, send nothing,
/// commit `Stage::Open`, react `Failed` and count `send_failed` — J-5:
/// never send what the operator cannot see, never enter `Confirming` on a
/// reading nobody was shown. Otherwise send `send` if any; on success
/// commit `stage` (with the echo id filled in by `Stage::with_echo` when
/// `post` was posted), react `react` and count `outcome`; on failure post
/// the daemon's error, commit `Stage::Open`, react `Failed`.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub post: Option<String>,
    pub send: Option<PluginAction>,
    pub stage: Stage,
    pub react: Option<Reaction>,
    /// The `inbound_total` outcome label, when this reply counts as one.
    pub outcome: Option<&'static str>,
}

impl Decision {
    /// Whether `post` is an echo that must be seen before anything is
    /// sent or confirmed. A refusal or a notice does not gate.
    pub fn gates_on_post(&self) -> bool {
        self.post.is_some()
            && (self.send.is_some() || matches!(self.stage, Stage::Confirming { .. }))
    }
}

fn refused(stage: Stage, post: String) -> Decision {
    Decision { post: Some(post), send: None, stage, react: Some(Reaction::Refused), outcome: Some("answer_refused") }
}

/// The keys for `selections` (`None` for a skip) as an action, or the
/// refusal the daemon would answer with, said here first (Spec J §7.5).
fn plan_action(
    questions: &[question::Question],
    selections: Option<&[Selection]>,
    key_delay_ms: u64,
) -> Result<PluginAction, String> {
    let steps: Vec<KeyStep> = match selections {
        Some(s) => question::plan(questions, s),
        None => question::skip_plan(),
    };
    let action = PluginAction::SendKeys { steps, delay_ms: key_delay_ms };
    action.validate().map_err(|reason| {
        format!("this answer needs more keystrokes than can be sent from here ({reason}); answer at the terminal.")
    })?;
    Ok(action)
}

/// Spec J §7.2 for one reply while `open` is the agent's question.
pub fn on_reply(open: &OpenQuestion, reply: &str, key_delay_ms: u64) -> Decision {
    let questions = &open.questions;
    match &open.stage {
        Stage::Sent { .. } => {
            return refused(open.stage.clone(), "an answer is already on its way; wait for the agent.".into());
        }
        Stage::Confirming { selections, echo } => match reply.trim().to_ascii_lowercase().as_str() {
            "yes" | "y" => {
                return match plan_action(questions, Some(selections), key_delay_ms) {
                    Ok(action) => Decision {
                        post: None,
                        send: Some(action),
                        stage: Stage::Sent { selections: Some(selections.clone()), echo: echo.clone() },
                        react: Some(Reaction::Ack),
                        outcome: Some("confirmed"),
                    },
                    Err(message) => refused(Stage::Open, message),
                };
            }
            "no" | "n" => {
                return Decision { post: None, send: None, stage: Stage::Open, react: Some(Reaction::Ack), outcome: None };
            }
            _ => {} // anything else is a fresh answer, matched below
        },
        Stage::Open => {}
    }

    match question::match_reply(questions, reply) {
        Err(Refusal(reason)) => refused(Stage::Open, reason),
        Ok(Matched::Skip) => match plan_action(questions, None, key_delay_ms) {
            Ok(action) => Decision {
                post: Some("**declining the question**".into()),
                send: Some(action),
                stage: Stage::Sent { selections: None, echo: None },
                react: Some(Reaction::Ack),
                outcome: Some("skipped"),
            },
            Err(message) => refused(Stage::Open, message),
        },
        Ok(Matched::Answers { selections, exact }) => {
            let chosen = question::describe(questions, &selections);
            if exact {
                match plan_action(questions, Some(&selections), key_delay_ms) {
                    Ok(action) => Decision {
                        post: Some(format!("**answering** {chosen}")),
                        send: Some(action),
                        stage: Stage::Sent { selections: Some(selections), echo: None },
                        react: Some(Reaction::Ack),
                        outcome: Some("answered"),
                    },
                    Err(message) => refused(Stage::Open, message),
                }
            } else {
                Decision {
                    post: Some(format!("**I read that as** {chosen}. Reply **yes** to send.")),
                    send: None,
                    stage: Stage::Confirming { selections, echo: None },
                    react: None,
                    outcome: Some("confirm_asked"),
                }
            }
        }
    }
}

/// What the `PostToolUse` that closes `open` says about the answer
/// (Spec J §7.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Recorded answers equal the intended ones: react `Confirmed` on the echo.
    Confirmed { echo: Option<String> },
    /// They differ: post this and count `answers_mismatched`.
    Mismatch { message: String },
    /// Nobody answered from the channel: post this when the question was shown.
    AnsweredAtTerminal { message: String },
    /// A skip was sent, or nothing to compare.
    Nothing,
}

pub fn on_closed(open: &OpenQuestion, answers: &Value) -> Verdict {
    let recorded = question::describe_recorded(&open.questions, answers);
    match &open.stage {
        Stage::Sent { selections: Some(selections), echo } => {
            if question::recorded_matches(&open.questions, selections, answers) {
                Verdict::Confirmed { echo: echo.clone() }
            } else {
                Verdict::Mismatch {
                    message: format!(
                        "**recorded answer differs** — Claude recorded {recorded}; you chose {}. Tell the agent if that matters.",
                        question::describe(&open.questions, selections)
                    ),
                }
            }
        }
        Stage::Sent { selections: None, .. } => Verdict::Nothing,
        Stage::Open | Stage::Confirming { .. } => {
            Verdict::AnsweredAtTerminal { message: format!("**answered at the terminal** {recorded}") }
        }
    }
}
```

Matrix's `deliver` validated the plan for a `yes` too (it called `deliver` with the held selections); the `Err(message) => refused(Stage::Open, message)` arms keep that. `Stage` needs `PartialEq` (it has it) and `Decision` derives `PartialEq` over `PluginAction` — confirm `PluginAction: PartialEq` in `balerix-api`; if not, drop `PartialEq` from `Decision` and compare fields in the tests.

- [ ] **Step 5: Run the common tests**

Run: `cd plugins/common && mise x -- cargo nextest run --config-file ../../.config/nextest.toml`
Expected: all pass. Fix any expected-string mismatch against `describe`'s real output by reading `question.rs`, not by changing `describe`.

- [ ] **Step 6: Commit**

```bash
git add plugins/common
git commit -m "feat(common): the answer flow as pure decisions, on_reply and on_closed (Spec K §4)"
```

---

### Task 9: Matrix's actor executes `answer` decisions

**Files:**
- Modify: `plugins/matrix/src/actor.rs` (`on_question_event`, `on_answer`, `deliver`, `echo_lost`)

**Interfaces:**
- Consumes: `answer::{on_reply, on_closed, Decision, Reaction, Verdict}`, `Stage::with_echo`.
- Produces: no public change. Every existing actor test passes unchanged.

- [ ] **Step 1: Run the actor tests as the baseline**

Run: `cd plugins/matrix && mise x -- cargo nextest run --config-file ../../.config/nextest.toml actor`
Expected: pass (note the count).

- [ ] **Step 2: Replace `on_answer`, `deliver` and `echo_lost` with the executor**

In `plugins/matrix/src/actor.rs`, add `use balerix_plugin_common::answer::{self, Decision, Reaction, Verdict};` and replace the three methods with:

```rust
    /// A thread reply while `agent` has a question open (Spec J §7.2):
    /// decide in common, execute here under `Decision`'s contract.
    async fn on_answer(&mut self, agent: &str, root: &str, message: &Inbound) {
        let Some(open) = self.questions.get(agent).cloned() else {
            return;
        };
        let delay_ms = self
            .agents
            .get(agent)
            .map(|c| c.key_delay_ms)
            .unwrap_or(balerix_api::DEFAULT_KEY_DELAY_MS);
        let decision = answer::on_reply(&open, &message.body, delay_ms);
        self.execute(agent, root, message, decision).await;
    }

    fn reaction_key(reaction: Reaction) -> &'static str {
        match reaction {
            Reaction::Ack => ACK,
            Reaction::Refused => REFUSED,
            Reaction::Failed => FAILED,
            Reaction::Confirmed => CONFIRMED,
        }
    }

    /// The executor contract of `balerix_plugin_common::answer::Decision`.
    async fn execute(&mut self, agent: &str, root: &str, message: &Inbound, d: Decision) {
        let count = |outcome: &str| self.counters.inbound.with_label_values(&[outcome]).inc();
        let mut echo = None;
        if let Some(body) = &d.post {
            match self.send(&message.room, Some(root), body, "question").await {
                Some(id) => echo = Some(id),
                None if d.gates_on_post() => {
                    // J-5: nothing may be sent, and no `Confirming` entered,
                    // on an echo the operator cannot see. The failed send
                    // already counted `errors{kind="send"}`; a notice would
                    // take the path that just failed, so the reaction is
                    // the whole report.
                    count("send_failed");
                    self.questions.set_stage(agent, Stage::Open);
                    self.react(message, FAILED).await;
                    return;
                }
                None => {}
            }
        }
        if let Some(action) = &d.send {
            if let Err(e) = self.host.action(agent, action).await {
                count("send_failed");
                self.counters.errors.with_label_values(&["send_keys"]).inc();
                self.questions.set_stage(agent, Stage::Open);
                let body = format!("**not delivered to {agent}:** {e}");
                self.send(&message.room, Some(root), &body, "notice").await;
                self.react(message, FAILED).await;
                return;
            }
        }
        let stage = if d.post.is_some() { d.stage.with_echo(echo) } else { d.stage };
        self.questions.set_stage(agent, stage);
        if let Some(outcome) = d.outcome {
            count(outcome);
        }
        if let Some(reaction) = d.react {
            self.react(message, Self::reaction_key(reaction)).await;
        }
    }
```

Then in `on_question_event`, replace the `Tracking::Closed(Some(open))` arm's body with:

```rust
            Tracking::Closed(Some(open)) => {
                let answers = event
                    .payload
                    .pointer("/tool_response/answers")
                    .cloned()
                    .unwrap_or(Value::Null);
                match answer::on_closed(&open, &answers) {
                    Verdict::Confirmed { echo } => {
                        if let Some(echo) = echo {
                            self.react_to(room, &echo, CONFIRMED).await;
                        }
                    }
                    Verdict::Mismatch { message } => {
                        // Posted whatever the filter says: it answers the
                        // operator's own action.
                        self.counters.answers_mismatched.inc();
                        if let Some(root) = root {
                            self.send(room, Some(&root), &message, "question").await;
                        }
                    }
                    Verdict::AnsweredAtTerminal { message } => {
                        if shown && let Some(root) = root {
                            self.send(room, Some(&root), &message, "question").await;
                        }
                    }
                    Verdict::Nothing => {}
                }
                true
            }
```

Remove the now-unused `use crate::question;` if nothing else in the file needs it (the tests use `question::fixtures` — keep it if so), and the unused `PluginAction`/`KeyStep` imports.

- [ ] **Step 3: Run the actor tests**

Run: `cd plugins/matrix && mise x -- cargo nextest run --config-file ../../.config/nextest.toml actor`
Expected: the same count as Step 1, all passing. In particular `an_exact_reply_is_echoed_then_sent_as_keys_and_never_as_text`, `an_inexact_reply_asks_first_and_yes_sends_it`, `no_drops_the_confirmation_and_another_reply_replaces_it`, `prose_is_refused_and_nothing_reaches_the_agent`, `skip_declines_with_one_escape`, `a_reply_while_keys_are_on_their_way_is_refused`, `a_failed_send_keys_is_reported_and_the_question_stays_open`, `an_answer_whose_echo_never_landed_sends_no_keys`, `a_confirmation_whose_echo_never_landed_is_not_entered`, `a_plan_too_long_for_the_agents_delay_is_refused_before_anything_is_sent`, `a_sent_answer_is_confirmed_on_the_echo_or_reported_when_it_differs`, `an_answer_given_at_the_terminal_is_reported`. If one fails, the executor deviates from the old ordering: compare against the old arm in `git show HEAD:plugins/matrix/src/actor.rs` and fix the executor, never the test.

- [ ] **Step 4: Run the full matrix check and the question verification**

Run: `mise run plugin matrix`
Expected: pass. Then, with a logged-in `claude`: `mise run verify-questions` — Expected: every dialog shape passes (the plans are produced by the same `question.rs`, so this proves the move, not new logic).

- [ ] **Step 5: Commit**

```bash
git add plugins/matrix
git commit -m "refactor(matrix): execute the answer flow from balerix-plugin-common's decisions (Spec K §4)"
```

---

### Task 10: The `common` release unit — `lib.sh`, `prepare.sh`, `plan.sh`, `affected-units.sh`

**Files:**
- Modify: `scripts/release/lib.sh`, `scripts/release/prepare.sh`, `scripts/release/plan.sh`, `scripts/release/affected-units.sh`, `scripts/release/test.sh`, `mise.toml:134-139`

**Interfaces:**
- Produces in `lib.sh`: `UNITS=(core common flow web matrix)`, `LIBRARY_UNITS=(common)`, `PLUGIN_UNITS=(flow web matrix)`, `IMAGE_UNITS=(core flow web matrix)`, `unit_kind <unit>` → `core|library|plugin`, `dep_version <manifest> <crate>` → the version a `{ version = "…" }` dependency names, `unit_paths common` = `plugins/common/**` + the two core crates, and a plugin's paths gaining `plugins/common/**` when its manifest names common.
- `plan.sh` emits a fourth output `crates=` (JSON list: `core` when planned, plus every planned library).

- [ ] **Step 1: Add the failing scenarios to `test.sh`**

Append before the scenario calls at the bottom of `scripts/release/test.sh`:

```bash
# A library unit releases like a plugin but ships crates, not a binary:
# common is proposed only once the SDK version its manifest names is
# tagged, and a core release moves that version.
scenario_common_ordering() {
  local dir out sdk
  dir=$(fixture common)
  sdk=$(dep_version_of "$dir" plugins/common/Cargo.toml balerix-plugin-sdk)
  if out=$(prepare "$dir" common 2>&1); then
    fail "common before core: prepare.sh should refuse while balerix-v$sdk is untagged"
  else
    pass "common before core: refused"
  fi
  expect_grep "common before core: names the core release" "balerix-v$sdk" <(echo "$out")
  release "$dir" core "$sdk"
  out=$(prepare "$dir" common)
  expect_eq "common after core: status" "$(field status "$out")" release
  expect_eq "common after core: tag" "$(field tag "$out")" "balerix-plugin-common-v$(manifest_version "$dir" common)"
  discard "$dir"
}

scenario_core_bump_moves_common() {
  local dir next
  dir=$(fixture common-core)
  release "$dir" core 0.4.0
  release "$dir" common 0.4.0
  change "$dir" crates/balerix-server/release-test.txt "feat!: a breaking daemon change"
  next=$(field version "$(prepare "$dir" core)")
  expect_eq "core bump: version" "$next" 0.5.0
  expect_eq "core bump: common's manifest names the new SDK" \
    "$(dep_version_of "$dir" plugins/common/Cargo.toml balerix-plugin-sdk)" "$next"
  expect_eq "core bump: common's manifest names the new API" \
    "$(dep_version_of "$dir" plugins/common/Cargo.toml balerix-api)" "$next"
  expect_eq "core bump: common's lockfile has the SDK at $next" \
    "$(lock_version "$dir/plugins/common/Cargo.lock" balerix-plugin-sdk)" "$next"
}

scenario_common_change_releases_dependents() {
  local dir unit
  dir=$(fixture common-dependents)
  release "$dir" core 0.4.0
  for unit in common flow web matrix; do release "$dir" "$unit" 0.4.0; done
  change "$dir" plugins/common/src/release-test.rs "fix(common): a shared fix"
  expect_eq "common change: common releases" "$(field status "$(prepare "$dir" common)")" release
  discard "$dir"
  expect_eq "common change: matrix releases" "$(field status "$(prepare "$dir" matrix)")" release
  discard "$dir"
  expect_eq "common change: web releases" "$(field status "$(prepare "$dir" web)")" release
  discard "$dir"
  expect_eq "common change: flow does not" "$(field status "$(prepare "$dir" flow)")" none
  expect_eq "common change: core does not" "$(field status "$(prepare "$dir" core)")" none
}

scenario_plan_crates() {
  local dir out
  dir=$(fixture plan-crates)
  release "$dir" core "$(dep_version_of "$dir" plugins/common/Cargo.toml balerix-plugin-sdk)"
  prepare "$dir" common >/dev/null
  gitc "$dir" add -A
  gitc "$dir" commit -qm "chore(release): common initial"
  out=$(plan "$dir")
  expect_eq "plan, merged common PR: units" "$(field units "$out")" '["common"]'
  expect_eq "plan, merged common PR: plugins" "$(field plugins "$out")" '[]'
  expect_eq "plan, merged common PR: binaries" "$(field binaries "$out")" '[]'
  expect_eq "plan, merged common PR: crates" "$(field crates "$out")" '["common"]'
  expect_eq "plan, merged common PR: core" "$(field core "$out")" false
}
```

Add the helper next to `lock_version`:

```bash
# The version a `{ version = "…" }` dependency names in <manifest>.
dep_version_of() {
  (
    cd "$1"
    # shellcheck source=scripts/release/lib.sh
    source scripts/release/lib.sh
    dep_version "$2" "$3"
  )
}
```

Extend `scenario_affected` with two lines:

```bash
  expect_eq "affected: a common change reaches the plugins built on it" \
    "$(affected_by "$dir" plugins/common/src/release-test.rs)" '["web","matrix"]'
```

(and keep every existing `'["core","flow","web","matrix"]'` expectation: images are built for `IMAGE_UNITS` only). In `assert_consistent`, add after the plugin loop:

```bash
  expect_eq "$label: common Cargo.lock has the SDK at $core" \
    "$(lock_version "$dir/plugins/common/Cargo.lock" balerix-plugin-sdk)" "$core"
```

Register the four scenarios at the bottom after `scenario_affected`. Note `fixture` deletes `plugins/*/CHANGELOG.md`, which covers `plugins/common`. The `release` helper works for `common` once `prepare.sh` does (Step 4); `scenario_common_ordering` releases core at the SDK version common names, which is why it reads `dep_version_of` first.

- [ ] **Step 2: Run the release tests to see them fail**

Run: `mise run release-test`
Expected: the new scenarios fail (`unknown release unit: 'common'`).

- [ ] **Step 3: Teach `lib.sh` the unit**

Replace the unit tables and `require_unit`, `unit_image`, `unit_paths`, and add `unit_kind` and `dep_version`:

```bash
# shellcheck disable=SC2034 # read by the scripts that source this file
UNITS=(core common flow web matrix)
# shellcheck disable=SC2034
PLUGIN_UNITS=(flow web matrix)
# Units that ship crates and no binary (Spec K §5).
# shellcheck disable=SC2034
LIBRARY_UNITS=(common)
# Units with a binary, an image and an archive: every unit but the libraries.
# shellcheck disable=SC2034
IMAGE_UNITS=(core flow web matrix)

require_unit() {
  case ${1:-} in
    core | common | flow | web | matrix) ;;
    *) die "unknown release unit: '${1:-}' (expected one of: ${UNITS[*]})" ;;
  esac
}

# core, library or plugin.
unit_kind() {
  require_unit "$1"
  case $1 in
    core) echo core ;;
    common) echo library ;;
    *) echo plugin ;;
  esac
}
```

`unit_crate`, `unit_tag_prefix`, `unit_tag`, `unit_manifest`, `unit_changelog` already answer `balerix-plugin-common`, `plugins/common/Cargo.toml` and `plugins/common/CHANGELOG.md` for `common` through their non-core branch; leave them. Change `unit_image`:

```bash
unit_image() {
  require_unit "$1"
  [[ $(unit_kind "$1") != library ]] || die "$1 is a library and has no image"
  local owner=${GITHUB_REPOSITORY_OWNER:-balerix-ai}
  echo "ghcr.io/${owner,,}/$(unit_crate "$1")"
}
```

and `unit_paths`:

```bash
# The paths whose commits count toward the unit, one per line. The SDK and
# the api are compiled into every plugin binary and into common, so they
# count for all of them; common is compiled into every plugin whose
# manifest names it, so it counts for those.
unit_paths() {
  require_unit "$1"
  if [[ $1 == core ]]; then
    printf '%s\n' 'crates/**' Cargo.toml Cargo.lock mise.toml
  else
    printf '%s\n' "plugins/$1/**" 'crates/balerix-api/**' 'crates/balerix-plugin-sdk/**'
    if [[ $(unit_kind "$1") == plugin ]] && grep -q '^balerix-plugin-common ' "plugins/$1/Cargo.toml"; then
      printf '%s\n' 'plugins/common/**'
    fi
  fi
}

# The version a dependency names in a manifest: the `version = "…"` inside
# `<crate> = { … }`. Empty when the dependency carries no version.
dep_version() {
  local manifest=$1 crate=$2
  sed -n "s/^$crate = {.*version = \"\([^\"]*\)\".*/\1/p" "$manifest" | head -n 1
}
```

- [ ] **Step 4: Teach `prepare.sh` the library rules**

After `require_unit "$unit"` add the ordering refusal:

```bash
# A library publishes to crates.io, where its `{ version, path }`
# dependencies must already exist: refuse until the core release that
# published that SDK version is tagged (Spec K §5).
if [[ $(unit_kind "$unit") == library ]]; then
  sdk=$(dep_version "$(unit_manifest "$unit")" balerix-plugin-sdk)
  [[ -n $sdk ]] || die "$unit: $(unit_manifest "$unit") names balerix-plugin-sdk without a version"
  tag_exists "$(unit_tag core "$sdk")" ||
    die "$unit: its manifest names balerix-plugin-sdk $sdk, which has no tag balerix-v$sdk yet;" \
      "release core $sdk first, then $unit"
fi
```

In the core branch that refreshes plugin lockfiles, also move common's versions:

```bash
if [[ $unit == core ]]; then
  for plugin in "${PLUGIN_UNITS[@]}"; do
    cargo update --manifest-path "plugins/$plugin/Cargo.toml" -p balerix-api -p balerix-plugin-sdk >&2
  done
  # A library names the two core crates by version for crates.io; the
  # version moves with core (Spec K §5).
  for library in "${LIBRARY_UNITS[@]}"; do
    manifest=$(unit_manifest "$library")
    for crate in balerix-api balerix-plugin-sdk; do
      sed -i "s/^\($crate = {.*version = \"\)[^\"]*\(\".*\)$/\1$next\2/" "$manifest"
    done
    cargo update --manifest-path "$manifest" -p balerix-api -p balerix-plugin-sdk >&2
  done
fi
```

Change the plugin-manifest `sed` so a library is skipped:

```bash
if [[ $(unit_kind "$unit") == plugin ]]; then
  sed -i "s/^version: .*/version: $next/" "plugins/$unit/package/balerix-plugin.yaml"
fi
```

The `cargo set-version --manifest-path "plugins/$unit/Cargo.toml"` branch already covers `common`.

- [ ] **Step 5: Teach `plan.sh` the new outputs**

```bash
units=()
plugins=()
binaries=()
crates=()
core=false
for unit in "${UNITS[@]}"; do
  …
  units+=("$unit")
  case $(unit_kind "$unit") in
    core) core=true; binaries+=("$unit"); crates+=("$unit") ;;
    plugin) plugins+=("$unit"); binaries+=("$unit") ;;
    library) crates+=("$unit") ;;
  esac
done

echo "units=$(json_list "${units[@]}")"
echo "plugins=$(json_list "${plugins[@]}")"
echo "binaries=$(json_list "${binaries[@]}")"
echo "crates=$(json_list "${crates[@]}")"
echo "core=$core"
```

Update the header comment to list the four outputs.

- [ ] **Step 6: Make `affected-units.sh` iterate image units**

Change both `"${UNITS[@]}"` occurrences to `"${IMAGE_UNITS[@]}"` and the header comment's first sentence to "Which image-bearing release units a change can break the image of".

- [ ] **Step 7: Update the mise task's help**

`mise.toml` `[tasks.release-prepare]` usage: `arg "<unit>" help="core, common, flow, web or matrix"`.

- [ ] **Step 8: Run the release tests**

Run: `mise run release-test`
Expected: every scenario passes, the four new ones included. `shellcheck -x scripts/release/*.sh` (part of `mise run lint`) passes.

- [ ] **Step 9: Commit**

```bash
git add scripts/release mise.toml
git commit -m "feat(release): the common library unit — versioned like a plugin, published like the core crates (Spec K §5)"
```

---

### Task 11: Publish and release workflows for a library unit

**Files:**
- Modify: `scripts/release/publish-crates.sh`, `scripts/release/github-release.sh`, `.github/workflows/release.yml`, `.github/workflows/release-pr.yml`, `docs/RELEASING.md`, `AGENTS.md:43-46`

**Interfaces:**
- `publish-crates.sh [--dry-run] <unit>...` publishes the crates of the named units: for `core`, `balerix-api` and `balerix-plugin-sdk` at the core version; for a library, its crate at its own version.

- [ ] **Step 1: Rewrite `publish-crates.sh` to take units**

```bash
#!/usr/bin/env bash
# Publishes the crates of the named units (Spec I §5.5, Spec K §5): core's
# balerix-api and balerix-plugin-sdk at the core version, and a library
# unit's crate at its own version. Skips any version crates.io already
# has, so a re-run is safe.
#
# usage: publish-crates.sh [--dry-run] <unit>...
# CARGO_REGISTRY_TOKEN must be set unless --dry-run is given.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

dry_run=false
if [[ ${1:-} == --dry-run ]]; then
  dry_run=true
  shift
fi
(($#)) || die "usage: $0 [--dry-run] <unit>..."

# The sparse index path of a crate name of four or more characters.
published() {
  local name=$1 version=$2
  curl -fsS "https://index.crates.io/${name:0:2}/${name:2:2}/$name" 2>/dev/null |
    jq -e --arg v "$version" 'select(.vers == $v)' >/dev/null
}

# One `cargo publish` per manifest: the core crates share the root
# workspace, a library is its own project.
publish() {
  local manifest=$1
  shift
  if $dry_run; then
    cargo publish --dry-run --locked --manifest-path "$manifest" "$@"
  else
    : "${CARGO_REGISTRY_TOKEN:?CARGO_REGISTRY_TOKEN must come from crates-io-auth-action}"
    cargo publish --locked --manifest-path "$manifest" "$@"
  fi
}

for unit in "$@"; do
  require_unit "$unit"
  version=$(unit_version "$unit")
  case $(unit_kind "$unit") in
    core)
      crates=()
      for crate in balerix-api balerix-plugin-sdk; do
        if published "$crate" "$version"; then
          echo "$crate $version is already on crates.io; skipping" >&2
        else
          crates+=(-p "$crate")
        fi
      done
      ((${#crates[@]})) && publish Cargo.toml "${crates[@]}"
      ;;
    library)
      crate=$(unit_crate "$unit")
      if published "$crate" "$version"; then
        echo "$crate $version is already on crates.io; skipping" >&2
      else
        publish "$(unit_manifest "$unit")"
      fi
      ;;
    *) die "$unit publishes no crates" ;;
  esac
done
```

(`cargo publish --manifest-path Cargo.toml -p a -p b` publishes several workspace crates in one call as before.)

- [ ] **Step 2: Let `github-release.sh` release a library without assets**

After `image=$(unit_image "$unit")` becomes conditional and the assets check moves under the same condition:

```bash
kind=$(unit_kind "$unit")
image=
[[ $kind == library ]] || image=$(unit_image "$unit")

[[ $kind == library || -f $assets/SHA256SUMS ]] || die "$unit: no SHA256SUMS in $assets"
```

Change the `if [[ $unit != core ]]` package block to `if [[ $kind == plugin ]]`, and add a library install note in the same place:

```bash
if [[ $kind == library ]]; then
  cat >>"$notes" <<EOF

### Use

\`\`\`sh
cargo add $crate@$version
\`\`\`
EOF
fi
```

Read the rest of the script (`### Verify` and the `gh release create` call) and make every reference to `$image` or to archive files conditional on `$kind != library`; a library's release is created with `--notes-file` and no asset arguments. Keep the existing behaviour for core and plugins byte-for-byte.

- [ ] **Step 3: Update `release.yml`**

- `plan` job outputs gain `binaries: ${{ steps.plan.outputs.binaries }}` and `crates: ${{ steps.plan.outputs.crates }}`.
- `build`, `images`, `merge-images`, `promote-images`: `if: needs.plan.outputs.binaries != '[]'` (where an `if` on `units` exists) and `unit: ${{ fromJSON(needs.plan.outputs.binaries) }}`.
- `publish-crates-dry-run` and `publish-crates`: replace `needs.plan.outputs.core == 'true'` with `needs.plan.outputs.crates != '[]'` in both `if`s, and their run steps become:
  ```yaml
      - env:
          CRATES: ${{ needs.plan.outputs.crates }}
        run: |
          mapfile -t units < <(jq -r '.[]' <<<"$CRATES")
          mise x -- scripts/release/publish-crates.sh --dry-run "${units[@]}"
  ```
  (the real job without `--dry-run`, keeping its `CARGO_REGISTRY_TOKEN` env).
- `github-release`: the archive download, the package download, the `Checksums` step and the attestation step gain `if: ${{ !contains(fromJSON(needs.plan.outputs.crates), matrix.unit) || matrix.unit == 'core' }}` (a library has none of them); the package download keeps `matrix.unit != 'core'` combined with it. Its `needs.build.result == 'success' && needs.merge-images.result == 'success'` condition becomes `(needs.build.result == 'success' || needs.build.result == 'skipped') && (needs.merge-images.result == 'success' || needs.merge-images.result == 'skipped')` so a library-only release (nothing to build) still releases.

Run `mise x -- actionlint && mise x -- zizmor --offline --min-severity medium .github` (both in `mise run lint`).

- [ ] **Step 4: Update `release-pr.yml`**

`options: [core, common, flow, web, matrix]` and the default matrix list `'["core","common","flow","web","matrix"]'`.

- [ ] **Step 5: Document**

`docs/RELEASING.md`:

- Units table gains, after core:
  `| common | \`balerix-plugin-common-v<ver>\` | \`balerix-plugin-common\` on crates.io |`
- After the table: "A library unit ships crates only: no binary, image or package. `common` names the SDK and API by version, so its release PR is refused (`release-prepare` dies naming the tag) until the core release that published that version is tagged; a core release moves those versions in `plugins/common/Cargo.toml`."
- One-time setup step 7 gains `-p`-style bootstrap for common: "and, once core's crates are published, from common's first release PR head: `mise x -- cargo publish --locked --manifest-path plugins/common/Cargo.toml`, plus its trusted publisher."
- "First releases" order becomes: flow, web, matrix, then common (after core).

`AGENTS.md` tasks entry for `release-prepare`: "(core, common, flow, web, matrix)", and the `plugin <name>` entry: "`plugins` does all four (common is the shared library, Spec K)". Add a gotcha under Gotchas:

```
- `plugins/common` is published to crates.io, so its `balerix-api` and
  `balerix-plugin-sdk` dependencies carry a version beside their path.
  `release-prepare core` moves them; `release-prepare common` refuses
  until that core version is tagged. In-tree plugins depend on common by
  path only (they are `publish = false`).
```

- [ ] **Step 6: Run the checks**

Run: `mise run lint && mise run release-test`
Expected: pass (actionlint, zizmor, shellcheck included).

- [ ] **Step 7: Commit**

```bash
git add scripts/release .github/workflows docs/RELEASING.md AGENTS.md
git commit -m "ci(release): publish library units to crates.io and release them without binaries (Spec K §5)"
```

---

### Task 12: README walkthrough and architecture notes

**Files:**
- Modify: `plugins/common/README.md`, `plugins/common/src/lib.rs`, `ARCHITECTURE.md:56-72`, `AGENTS.md:99,366`

**Interfaces:**
- Produces: a README whose code compiles as a doc test (`#![doc = include_str!("../README.md")]` in `lib.rs`).

- [ ] **Step 1: Write the README**

`plugins/common/README.md`:

````markdown
# balerix-plugin-common

Shared building blocks for [balerix](https://github.com/balerix-ai/balerix)
chat plugins: rendering hook events as markdown, splitting long bodies,
showing and answering Claude's `AskUserQuestion` dialogs, config helpers,
a drop-oldest command queue, the shared metric families and phase diffing.
The matrix plugin is built on it; so is the GitHub plugin. This crate pairs
with the `balerix-plugin-sdk` and `balerix-api` versions its manifest names.

The wire contract a plugin speaks is
[`docs/plugin-protocol.md`](https://github.com/balerix-ai/balerix/blob/main/docs/plugin-protocol.md).

## An observer plugin in one page

A plugin has three parts: a `Plugin` impl that validates and enqueues, an
actor task that owns every piece of state, and a channel adapter. The
first two are almost entirely this crate.

```rust
use std::sync::Arc;

use balerix_api::HookEvent;
use balerix_plugin_common::answer::{self, Decision};
use balerix_plugin_common::config::{EventFilter, deserialize, ConfigError};
use balerix_plugin_common::metrics::Shared;
use balerix_plugin_common::pending::{Questions, Stage};
use balerix_plugin_common::phases::PhaseChange;
use balerix_plugin_common::queue::{Health, Queue};
use balerix_plugin_common::{question, render};
use balerix_plugin_sdk::{Host, Metrics, Plugin};
use serde_json::Value;

/// The per-agent block: `plugins.<name>` in the fleet file.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
struct AgentConfig {
    enabled: bool,
    events: EventFilter,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self { enabled: true, events: EventFilter::default() }
    }
}

fn parse_agent(config: &Value) -> Result<AgentConfig, ConfigError> {
    let c: AgentConfig = deserialize(config)?;
    c.events.validate()?;
    Ok(c)
}

enum Command {
    Activate { agent: String, config: AgentConfig },
    Deactivate { agent: String },
    Events(Vec<HookEvent>),
    Phases(Vec<PhaseChange>),
    Reply { agent: String, body: String },
}

struct MyPlugin {
    metrics: Metrics,
    health: Health,
    queue: Arc<Queue<Command>>,
}

impl Plugin for MyPlugin {
    async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
        let config = parse_agent(&config).map_err(|e| e.to_string())?;
        self.queue.push(Command::Activate { agent: agent.to_string(), config });
        Ok(())
    }
    async fn deactivate(&self, agent: &str) {
        self.queue.push(Command::Deactivate { agent: agent.to_string() });
    }
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

/// What the actor does with one hook event: track questions first, then
/// render whatever the agent's filter wants.
fn on_event(questions: &mut Questions, config: &AgentConfig, event: &HookEvent) -> Option<String> {
    if let Some(input) = event.payload.get("tool_input")
        && event.name == "PreToolUse"
        && event.payload.get("tool_name").and_then(Value::as_str) == Some(question::TOOL)
        && let Some(parsed) = question::parse(input)
    {
        // Persisting through `Questions::open` needs a `Host`; the actor
        // holds one. Rendering is pure:
        return Some(render::question_message(&parsed));
    }
    config.enabled.then(|| config.events.wants(&event.name)).unwrap_or(false)
        .then(|| render::event_message(event))
}

/// What the actor does with a reply while a question is open: decide here,
/// then post `d.post`, send `d.send`, commit `d.stage`, react `d.react`,
/// in that order and under the contract in `Decision`'s docs.
fn on_reply(open: &balerix_plugin_common::pending::OpenQuestion, body: &str) -> Decision {
    answer::on_reply(open, body, balerix_api::DEFAULT_KEY_DELAY_MS)
}

fn main() {
    let metrics = Metrics::new("mine");
    let shared = Shared::new(&metrics).expect("fresh registry");
    let queue: Arc<Queue<Command>> = Queue::new(shared.events_dropped.clone());
    let _plugin = MyPlugin { metrics, health: Health::new(), queue };
    // `balerix_plugin_sdk::serve(&host, env!("CARGO_PKG_VERSION"), plugin)`
    // says hello and serves; spawn the actor and
    // `balerix_plugin_common::phases::run(host, |c| queue.push(Command::Phases(c)))`
    // beside it.
    let _ = (on_event, on_reply, Stage::Open);
}
```

Long bodies: `render::split(text, limit, max_parts)` breaks at line
boundaries, reopens code fences across parts and marks each part `(n/N)`.
Reviews: `review::render_message` turns a `review::Review` into the one
message the web plugin and the GitHub plugin deliver.

## Testing your plugin

`balerix_plugin_sdk::testing::{FakeHost, Harness}` serve the real router
over HTTP. `question::fixtures` holds dialogs to build tests on. Write your
channel as a port trait with a recording fake, as the matrix plugin's
`matrix.rs` does, and test the actor against it.
````

Add `#![doc = include_str!("../README.md")]` at the top of `lib.rs` (after the `//!` header, or replacing it with the README as the crate docs). The README's code block then compiles as a doc test; it uses `let _ = …` to keep every item referenced. If the `Plugin` trait has more required methods than shown, add them from `crates/balerix-plugin-sdk/src/plugin.rs` so the doc test compiles.

- [ ] **Step 2: Run the doc test**

Run: `cd plugins/common && mise x -- cargo test --doc`
Expected: the README block compiles and passes.

- [ ] **Step 3: Architecture and gotcha updates**

`ARCHITECTURE.md`, in "The pieces", before `balerix-plugin-flow`:

```
- `balerix-plugin-common` — the published library the chat plugins are
  built on (Spec K): `render` (events to markdown, the fence-aware
  `split`), `question` and `pending` (the `AskUserQuestion` dialog, its
  matching and key plan, the open question mirrored to KV), `answer` (the
  answer flow as a pure decision the plugin's actor executes), `config`
  helpers, `queue`, `metrics::Shared`, `phases` and `review`. A standalone
  project under `plugins/common/`, published to crates.io as its own
  release unit.
```

In the `balerix-plugin-matrix` bullet, change "Everything the plugin knows about Claude's dialog is in `question.rs`" to "Everything about Claude's dialog is in common's `question.rs`; the matrix actor executes common's `answer` decisions".

`AGENTS.md`: line 99's path becomes `plugins/common/src/question.rs::plan`; line 366's becomes `plugins/common/src/queue.rs`, used by `plugins/matrix/src/actor.rs`; line 36's "the matrix plugin's `question.rs`" becomes "common's `question.rs` (`plugins/common`)".

- [ ] **Step 4: Full verification**

Run: `mise run plugins && mise run check && mise run release-test`
Expected: all green; `git status` shows no `.snap.new`.

- [ ] **Step 5: Commit**

```bash
git add plugins/common ARCHITECTURE.md AGENTS.md
git commit -m "docs(common): the build-a-plugin walkthrough as a doc test, and the architecture map (Spec K §8)"
```

---

## Self-review against the spec

- **§3 crate and modules:** Task 1 (scaffold), 2 (`config`), 3 (`queue`), 4 (`metrics`), 5 (`render`, `question`, `pending`), 6 (`phases`), 7 (`review`), 8 (`answer`). `lib.rs` re-exports through `pub mod`. Every pub item is doc-commented in the code shown; the moved modules already are.
- **§3.1 table:** `split(limit)` (Task 5), `PhaseChange` in `phases` re-exported by `render` (Task 6), `pending` log prefix (Task 5 Step 2), `deserialize` pub (Task 2), `EventFilter`/`validate_key_delay` (Task 2), `Queue`/`Health` (Task 3), `Shared` (Task 4), `phase_changes`+`run` (Task 6), `ReviewBody`→`Review` with a web alias (Task 7). Matrix's `AgentConfig` and `DaemonConfig` stay in matrix (Task 2).
- **§4 answer:** Task 8 implements every arm and text; Task 9 collapses `on_answer`/`deliver`/`echo_lost` into the executor with the J-5 rule via `gates_on_post`.
- **§5 release:** Task 10 (units, ordering refusal, core moving common's versions, `plan.sh` outputs, affected units over image units, scenarios), Task 11 (`publish-crates.sh` per unit, `github-release.sh` without assets, workflows, RELEASING.md, the bootstrap step, AGENTS.md).
- **§6 matrix and web changes:** Tasks 2–7, 9; `examples/question_plan.rs` compiles through the re-export (Task 5 Step 5); AGENTS.md paths (Task 12).
- **§7 testing:** moved tests move with their modules; `answer` example tests (Task 8); `EventFilter` tests (Task 2); `phases::run` against `FakeHost` (Task 6); web's moved review tests (Task 7); `cargo package` in `lint` and `cargo deny` under `plugins/common` (Task 1); the regression gate runs at the end of every matrix-touching task and in Task 12 Step 4; `verify-questions` in Task 9 Step 4.
- **§8 README:** Task 12, compiled as a doc test.
- **§10 done-when:** 1 (Task 1, 12), 2 (Tasks 5, 9, 12), 3 (Task 9), 4 (Tasks 10, 11), 5 (Task 12), 6 (Task 12).
- **Type consistency:** `Queue<C>` with `Queue::new(IntCounter) -> Arc<Self>` in Tasks 3, 12; `Shared` fields in Tasks 4, 9, 12; `Decision { post, send, stage, react, outcome }` and `gates_on_post` in Tasks 8, 9, 12; `Stage::with_echo` defined in Task 8 Step 3 and used in Tasks 8, 9; `split(text, limit, max_parts)` in Tasks 5, 12; `phases::run(host, sink)` in Tasks 6, 12; `unit_kind`, `dep_version`, `IMAGE_UNITS`, `LIBRARY_UNITS` in Tasks 10, 11; `plan.sh` outputs `binaries`/`crates` in Tasks 10, 11.
