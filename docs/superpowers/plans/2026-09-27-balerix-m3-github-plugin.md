# Spec M part 3: The GitHub Plugin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A new standalone plugin `balerix-plugin-github` that turns a mention of a GitHub App on an issue or pull request into a running agent whose session is that issue: Claude's turns and questions post as comments, permitted comments become prompts or answers, a submitted review reaches the agent as one message, the fleet is built from the repository's own `.balerix.yaml`, and the session ends on close, merge or idle.

**Architecture:** The matrix shape (Spec G-13): a `Plugin` impl that validates and enqueues, one actor task that owns every piece of state, and a channel adapter behind a port trait with a recording fake. Here the channel is GitHub: a webhook listener (axum, HMAC-verified, deduplicated) enqueues `Command::Webhook`; a thin `reqwest` client implements `GitHubPort` (App JWT, installation tokens, ten endpoints); pure modules do mention detection, `.balerix.yaml` validation and agent injection, the first prompt, the review message and the status comment. Common supplies questions, answers, phases, the queue, the metrics, splitting and delivery confirmation (part 2). The daemon side from part 1 supplies the restricted surface and the fleets-gated `PUT`.

**Tech Stack:** Rust 1.98 (edition 2024), `reqwest` (rustls), `jsonwebtoken` (RS256), `hmac`+`sha2`, `axum`, `serde_norway`, `base64`, tokio, insta; the release pipeline's plugin unit machinery.

**Spec:** `docs/superpowers/specs/2026-09-22-balerix-m-github-plugin-design.md` (every section; §12.1/§12.2 are part 1's, §8.7's common half is part 2's). Parts 1 and 2 must be on `main` first: this plan calls `Host::apply_fleet -> Option<FleetRecord>` and `common::delivery`.

## Global Constraints

- The plugin is a standalone project under `plugins/github/` with its own `[workspace]`, `Cargo.lock`, `clippy.toml` and `deny.toml`, depending on `balerix-api`, `balerix-plugin-sdk` and `balerix-plugin-common` by path only (`publish = false`, AGENTS.md; Spec M §3's `{ version, path }` is superseded: only common carries versions). Run its checks with `mise run plugin github`; `mise run check` is unaffected until the plumbing task.
- Every new dependency is exact and lands in `plugins/github/Cargo.toml`, with the reason in the commit. This plan adds: `reqwest` (`default-features = false`, features `rustls`, `json`), `jsonwebtoken`, `hmac`, `sha2`, `base64`, `serde_norway`, `axum`, plus the boilerplate matrix has (`serde`, `serde_json`, `serde_path_to_error`, `thiserror`, `tokio`, `anyhow`, `tracing`, `tracing-subscriber`; dev `insta`). Versions: use the root workspace's where it pins the crate (`serde 1.0.229`, `serde_json 1.0.151`, `serde_norway 0.9.42`, `thiserror 2.0.20`, `anyhow 1.0.104`, `sha2 0.11.0`, `tokio 1.53.1`, `axum 0.8.9`, `reqwest 0.13.4`, `tracing 0.1.44`, `tracing-subscriber 0.3.23`, `insta 1.48.0`, `serde_path_to_error 0.1.20`); for `jsonwebtoken`, `hmac` and `base64`, pin what `mise x -- cargo search <crate> --limit 1` prints at the time (expected `hmac 0.13.x`, the one paired with `sha2 0.11`; `base64 0.22.x`; `jsonwebtoken`'s current major).
- `unsafe_code = "forbid"`; clippy `all` warn, `unwrap_used`/`expect_used` warn outside tests; `clippy.toml` allows them in tests.
- Secrets (`privateKey`, `webhookSecret`, installation tokens) live in `Secret` newtypes, never reach KV, logs, comments or error messages; `Debug` prints `<redacted>`.
- Everything the plugin posts to GitHub goes through common's `render::split(text, 65_536, maxParts)` for bodies and `one_line` treatment for titles and labels (GitHub's comment limit is 65 536 characters).
- The plugin's process prints `github: …` to stderr and exits 1 on any startup failure (the release scripts grep `^github: `).
- Time inside the actor is `tokio::time::Instant` for windows and caches; wall-clock seconds (`Timestamp`) for rows and the status comment.
- The daemon answers `apply_fleet` with the record (this plugin declares `fleets`); `None` is treated as success all the same.
- Commit after every task. The PR title is `feat(github): the GitHub plugin, an agent per issue or pull request (Spec M)`; it touches only `plugins/github/` plus the plumbing files of Task 11, so it releases nothing until `release-prepare github` opens the first release PR (0.1.0). Do not commit a `plugins/github/CHANGELOG.md` with a released section (the release scripts would ship it on merge); leave the file out.

## Review Focus

1. **A mention inside a fenced code block or inline code** (`` `@balerix` ``, ```` ``` @balerix ``` ````) must not start a session. Test: `mentions_inside_code_are_not_mentions` in Task 3.
2. **A comment whose author is the App itself** (its own status comment edits, its own turn comments arrive as `issue_comment` webhooks) must be silent, never a prompt to the agent. Test: `the_apps_own_comments_are_silent` in Task 7.
3. **A repository renamed after its fleet exists** (`repo/<fleet>` in KV names the old `owner/name`) must be refused with the spec's message until the operator clears the key, never start a second fleet under the same sanitised name. Test: `a_name_collision_is_refused` in Task 7.
4. **A `Stop` whose `last_assistant_message` exceeds one comment** must post its parts in order and stop at the first failure, never post the tail without its head. Test: `a_long_turn_is_split_and_a_failed_part_stops_the_rest` in Task 8.
5. **A webhook delivered twice** (GitHub retries on a slow 2xx) must be handled once: the delivery-id ring answers 202 and enqueues nothing. Test: `a_replayed_delivery_is_counted_and_dropped` in Task 5.

---

## File Structure

**Created** (all under `plugins/github/` unless noted)

- `Cargo.toml`, `Cargo.lock`, `clippy.toml`, `deny.toml`, `package/balerix-plugin.yaml`, `package/mise.toml` — the standalone project and its package.
- `src/main.rs` — env, tracing, build, serve; `github: …` and exit 1 on failure.
- `src/lib.rs` — the modules and re-exports.
- `src/config.rs` — `DaemonConfig` (§4.1), `AgentConfig` (§4.2), `Kind`.
- `src/github.rs` — `GitHubPort`, `GitHubError`, `Permission`, `Target`, `ReviewComment`, `fake::FakePort`.
- `src/client.rs` — the `reqwest` implementation: App JWT, installation tokens, the endpoints, `Retry-After`.
- `src/webhook.rs` — the axum listener: signature, dedupe ring, `WebhookEvent`, `parse`.
- `src/mention.rs` — mention detection, fleet-name sanitising, agent names.
- `src/repo_config.rs` — `.balerix.yaml` validation and agent injection (§6).
- `src/prompt.rs` — the first prompt (§8.2) and the review message (§9).
- `src/status.rs` — the status comment body (§8.3).
- `src/session.rs` — `Session` rows, `Sessions` mirrored to KV, the reverse map, `repo/<fleet>`.
- `src/actor.rs` — `Command`, `Counters`, `Actor`: the state and the command loop.
- `src/plugin.rs` — `GitHubPlugin<L: Launcher>`, the SDK surface.
- `src/snapshots/` — insta snapshots.
- `tests/plugin_it.rs` — the real `Plugin` over the wire with `FakeHost`, `FakePort` and a signed webhook.
- `scripts/verify-github.sh` — the by-hand check against a real App.

**Modified** (Task 11)

- `mise.toml`, `scripts/package-plugins.sh`, `.github/workflows/ci.yml`, `.github/workflows/release-pr.yml`, `.github/actions/{build-binary,build-image}/action.yml`, `scripts/release/lib.sh`, `scripts/release/test.sh`, `.vscode/settings.json`, `docs/RELEASING.md`, `AGENTS.md`, `ARCHITECTURE.md`, `README.md`, `plugins/common/README.md`, `docs/THREAT-MODEL.md`, the Spec M file (§18).

---

### Task 1: The project skeleton and `config.rs`

**Files:**
- Create: `plugins/github/Cargo.toml`, `clippy.toml`, `deny.toml`, `package/balerix-plugin.yaml`, `package/mise.toml`, `src/main.rs`, `src/lib.rs`, `src/config.rs`.

**Interfaces:**
- Produces:
  - `config::DaemonConfig { app_id: u64, private_key: Secret, webhook_secret: Secret, listen: SocketAddr, config_path: String, idle_timeout: Duration, max_parts: usize }`, `parse_daemon(&Value) -> Result<DaemonConfig, ConfigError>`.
  - `config::AgentConfig { enabled: bool, events: EventFilter, phases: bool, key_delay_ms: u64, confirm_window: Duration, kind: Option<Kind>, number: Option<u64> }`, `parse_agent(&Value) -> Result<AgentConfig, ConfigError>` (requires `kind` and `number`), `AgentConfig::wants`.
  - `config::Kind { Issue, Pr }` (serde `issue`/`pr`), `Kind::agent_name(self, number) -> String` (`issue-12`, `pr-34`).
  - Constants `DEFAULT_LISTEN`, `DEFAULT_CONFIG_PATH`, `DEFAULT_IDLE_TIMEOUT`, `DEFAULT_MAX_PARTS`, `COMMENT_LIMIT = 65_536`.

- [ ] **Step 1: The project files**

`plugins/github/Cargo.toml`:

```toml
# A standalone project, not a workspace member (Spec H): its TLS and
# HTTP tree stays out of the daemon's feature resolution.
[workspace]
resolver = "3"

[package]
name = "balerix-plugin-github"
description = "The GitHub plugin: a mention of a GitHub App on an issue or pull request starts an agent whose session is that issue (Spec M)"
version = "0.1.0"
edition = "2024"
rust-version = "1.98"
license = "Apache-2.0"
repository = "https://github.com/balerix-ai/balerix"
publish = false

[[bin]]
name = "balerix-plugin-github"
path = "src/main.rs"

[dependencies]
balerix-api = { path = "../../crates/balerix-api" }
balerix-plugin-sdk = { path = "../../crates/balerix-plugin-sdk" }
balerix-plugin-common = { path = "../common" }
# The GitHub REST client: ten endpoints, TLS through rustls (M-12).
reqwest = { version = "0.13.4", default-features = false, features = ["rustls", "json"] }
# The App JWT (RS256) that mints installation tokens.
jsonwebtoken = "<cargo search>"
# Webhook signatures: HMAC-SHA256 over the raw body.
hmac = "<cargo search>"
sha2 = "0.11.0"
# `contents` API bodies are base64.
base64 = "<cargo search>"
# `.balerix.yaml` is parsed here (the core's YAML crate).
serde_norway = "0.9.42"
# The webhook listener; already in the SDK's tree.
axum = "0.8.9"
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
serde_path_to_error = "0.1.20"
thiserror = "2.0.20"
tokio = { version = "1.53.1", features = ["rt-multi-thread", "macros", "sync", "time", "signal", "net"] }
anyhow = "1.0.104"
tracing = "0.1.44"
tracing-subscriber = { version = "0.3.23", features = ["env-filter"] }

[dev-dependencies]
insta = { version = "1.48.0", features = ["yaml", "json"] }

[lints.rust]
unsafe_code = "forbid"

[lints.clippy]
all = { level = "warn", priority = -1 }
unwrap_used = "warn"
expect_used = "warn"

[profile.release]
strip = true
```

Replace each `<cargo search>` with the exact version `mise x -- cargo search <crate> --limit 1` prints. If `cargo build` refuses the `rustls` feature name on reqwest, `mise x -- cargo info reqwest` lists the feature names; the rustls one is what the sibling `plugins/matrix/deny.toml` comment calls "reqwest's `rustls` feature".

`plugins/github/clippy.toml`:

```toml
allow-unwrap-in-tests = true
allow-expect-in-tests = true
```

`plugins/github/deny.toml`: copy `plugins/matrix/deny.toml`, drop the `BSL-1.0` entry and the matrix-specific comments, keep `CDLA-Permissive-2.0` with the comment "The Mozilla root certificate bundle, as data: `webpki-root-certs` through rustls-platform-verifier, which reqwest's `rustls` feature pulls in." Any other licence `cargo deny` then refuses gets an entry with a comment naming the crate and why the licence is acceptable, as matrix's does.

`plugins/github/package/balerix-plugin.yaml`:

```yaml
apiVersion: balerix/v1
kind: Plugin
name: github
version: 0.1.0
protocol: 1
start: serve
# Every event is observed: the set is per-agent config (Spec M §4.2), so
# the filtering is the plugin's job, not the subscription's.
hooks:
  observe: [SessionStart, SessionEnd, UserPromptSubmit, PreToolUse, PostToolUse, Notification, Stop, SubagentStop, PreCompact]
  intercept: []
# fleets: readiness and phases. actions: send_text and send_keys. kv:
# session rows. manage: the fleet per repository (Spec L).
needs: [fleets, actions, kv, manage]
routes: false
```

`plugins/github/package/mise.toml`: matrix's, with `run = "./bin/balerix-plugin-github"`.

- [ ] **Step 2: `lib.rs` and `main.rs`**

`src/lib.rs`:

```rust
//! The GitHub plugin (Spec M): a mention of the App on an issue or pull
//! request starts an agent whose session is that issue. Turns and
//! questions post as comments; permitted comments come back as prompts
//! or answers; a submitted review is one message; the fleet is the
//! repository's own `.balerix.yaml`.

pub mod actor;
pub mod client;
pub mod config;
pub mod github;
pub mod mention;
pub mod plugin;
pub mod prompt;
pub mod repo_config;
pub mod session;
pub mod status;
pub mod webhook;

pub use balerix_plugin_common::{delivery, pending, phases, question, render, review};
pub use plugin::{GitHubPlugin, Launcher};
```

(Add each `pub mod` as its task creates the file; until then leave the line out so the crate builds.)

`src/main.rs`: matrix's `main.rs` with `github` in place of `matrix`, `GitHubPlugin` and `client::GitHubLauncher`, and no tick task here (the launcher spawns it, Task 10, so the webhook listener and the ticker share the launcher's lifetime).

- [ ] **Step 3: Write the failing config tests**

`src/config.rs` tests module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn daemon(extra: serde_json::Value) -> serde_json::Value {
        let mut v = json!({ "appId": 12345, "privateKey": "-----BEGIN RSA PRIVATE KEY-----\nx\n-----END RSA PRIVATE KEY-----", "webhookSecret": "s3cret" });
        if let (Some(m), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            m.extend(e.clone());
        }
        v
    }

    #[test]
    fn daemon_config_defaults_and_redacts_its_secrets() {
        let c = parse_daemon(&daemon(json!({}))).unwrap();
        assert_eq!(c.app_id, 12345);
        assert_eq!(c.listen.to_string(), "127.0.0.1:8787");
        assert_eq!(c.config_path, ".balerix.yaml");
        assert_eq!(c.idle_timeout, Duration::from_secs(7200));
        assert_eq!(c.max_parts, 10);
        assert_eq!(c.webhook_secret.expose(), "s3cret");
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("s3cret") && !dbg.contains("BEGIN RSA"), "{dbg}");
    }

    #[test]
    fn daemon_config_reads_every_knob_and_names_bad_ones() {
        let c = parse_daemon(&daemon(json!({
            "listen": "0.0.0.0:9000", "configPath": ".github/balerix.yaml",
            "idleTimeout": "0", "maxParts": 3
        })))
        .unwrap();
        assert_eq!(c.listen.to_string(), "0.0.0.0:9000");
        assert_eq!(c.config_path, ".github/balerix.yaml");
        assert_eq!(c.idle_timeout, Duration::ZERO);
        assert_eq!(c.max_parts, 3);
        for (extra, path) in [
            (json!({ "listen": "nowhere" }), "listen"),
            (json!({ "idleTimeout": "soon" }), "idleTimeout"),
            (json!({ "maxParts": 0 }), "maxParts"),
            (json!({ "configPath": "" }), "configPath"),
            (json!({ "configPath": "/abs" }), "configPath"),
            (json!({ "nope": 1 }), "nope"),
        ] {
            assert_eq!(parse_daemon(&daemon(extra)).unwrap_err().path, path);
        }
        let e = parse_daemon(&json!({ "appId": 1 })).unwrap_err();
        assert_eq!(e.path, "privateKey");
    }

    #[test]
    fn agent_config_requires_kind_and_number_and_defaults_the_rest() {
        let c = parse_agent(&json!({ "kind": "issue", "number": 12 })).unwrap();
        assert!(c.enabled && c.phases);
        assert_eq!(c.key_delay_ms, 100);
        assert_eq!(c.confirm_window, Duration::from_secs(30));
        assert_eq!((c.kind, c.number), (Some(Kind::Issue), Some(12)));
        assert!(c.wants("Stop") && c.wants("SessionStart") && !c.wants("PreToolUse"));
        assert_eq!(parse_agent(&json!({ "kind": "pr" })).unwrap_err().to_string(), "number: missing");
        assert_eq!(parse_agent(&json!({ "number": 3 })).unwrap_err().to_string(), "kind: missing");
        assert_eq!(
            parse_agent(&json!({ "kind": "gist", "number": 3 })).unwrap_err().path,
            "kind"
        );
        assert_eq!(
            parse_agent(&json!({ "kind": "pr", "number": 0 })).unwrap_err().to_string(),
            "number: must be at least 1"
        );
        assert_eq!(Kind::Issue.agent_name(12), "issue-12");
        assert_eq!(Kind::Pr.agent_name(34), "pr-34");
    }

    #[test]
    fn a_disabled_agent_still_needs_its_number() {
        assert!(parse_agent(&json!({ "enabled": false })).is_err());
    }
}
```

- [ ] **Step 4: Run to verify failure**

Run: `cd plugins/github && mise x -- cargo nextest run config`
Expected: FAIL to compile.

- [ ] **Step 5: Implement `config.rs`**

```rust
//! The plugin's two config blocks (Spec M §4). The daemon has already
//! read any file-backed secret (`secrets.privateKey`,
//! `secrets.webhookSecret`), so both are plain values here.

use std::net::SocketAddr;
use std::time::Duration;

pub use balerix_plugin_common::config::{
    ConfigError, DEFAULT_EVENTS, EventFilter, LIFECYCLE, Secret, deserialize, deserialize_duration,
    validate_key_delay,
};
use balerix_api::DEFAULT_KEY_DELAY_MS;
use serde::Deserialize;
use serde_json::Value;

/// Where the webhook listener binds unless `listen` says otherwise.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8787";
/// The fleet file's path in the repository, on the default branch.
pub const DEFAULT_CONFIG_PATH: &str = ".balerix.yaml";
/// Idle sessions end after this; `0` disables.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(2 * 3600);
/// Most comments one turn may become.
pub const DEFAULT_MAX_PARTS: usize = 10;
/// GitHub's comment body limit, in characters.
pub const COMMENT_LIMIT: usize = 65_536;
/// How long a prompt may go without `UserPromptSubmit` (Spec M §8.7).
pub const DEFAULT_CONFIRM_WINDOW: Duration = Duration::from_secs(30);

/// Spec M §4.1.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DaemonConfig {
    pub app_id: u64,
    pub private_key: Secret,
    pub webhook_secret: Secret,
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    #[serde(default = "default_config_path")]
    pub config_path: String,
    #[serde(default = "default_idle", deserialize_with = "deserialize_duration")]
    pub idle_timeout: Duration,
    #[serde(default = "default_max_parts")]
    pub max_parts: usize,
}

impl std::fmt::Debug for DaemonConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonConfig")
            .field("app_id", &self.app_id)
            .field("private_key", &"<redacted>")
            .field("webhook_secret", &"<redacted>")
            .field("listen", &self.listen)
            .field("config_path", &self.config_path)
            .field("idle_timeout", &self.idle_timeout)
            .field("max_parts", &self.max_parts)
            .finish()
    }
}

fn default_listen() -> SocketAddr {
    // The constant is well-formed; a parse failure here is a programming error.
    DEFAULT_LISTEN.parse().unwrap_or(SocketAddr::from(([127, 0, 0, 1], 8787)))
}
fn default_config_path() -> String {
    DEFAULT_CONFIG_PATH.to_string()
}
fn default_idle() -> Duration {
    DEFAULT_IDLE_TIMEOUT
}
fn default_max_parts() -> usize {
    DEFAULT_MAX_PARTS
}

/// Parses and checks the `hello` config.
pub fn parse_daemon(config: &Value) -> Result<DaemonConfig, ConfigError> {
    let c: DaemonConfig = deserialize(config)?;
    let invalid = |path: &str, message: &str| ConfigError {
        path: path.into(),
        message: message.into(),
    };
    if c.max_parts == 0 {
        return Err(invalid("maxParts", "must be at least 1"));
    }
    if c.config_path.is_empty() || c.config_path.starts_with('/') || c.config_path.contains("..") {
        return Err(invalid("configPath", "a relative path inside the repository"));
    }
    if c.private_key.expose().trim().is_empty() {
        return Err(invalid("privateKey", "must not be empty"));
    }
    if c.webhook_secret.expose().is_empty() {
        return Err(invalid("webhookSecret", "must not be empty"));
    }
    Ok(c)
}

/// Issue or pull request: what an agent is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Issue,
    Pr,
}

impl Kind {
    /// The agent's name in the crew (M-1): `issue-<n>` / `pr-<n>`.
    pub fn agent_name(self, number: u64) -> String {
        match self {
            Kind::Issue => format!("issue-{number}"),
            Kind::Pr => format!("pr-{number}"),
        }
    }
}

/// Spec M §4.2: the agent's `plugins.github` block. `kind` and `number`
/// are what the plugin injected when it added the agent; a hand-written
/// agent in `.balerix.yaml` lacks them and is refused.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentConfig {
    pub enabled: bool,
    pub events: EventFilter,
    pub phases: bool,
    #[serde(rename = "keyDelayMs")]
    pub key_delay_ms: u64,
    #[serde(rename = "confirmWindow", deserialize_with = "deserialize_duration")]
    pub confirm_window: Duration,
    pub kind: Option<Kind>,
    pub number: Option<u64>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            events: EventFilter::default(),
            phases: true,
            key_delay_ms: DEFAULT_KEY_DELAY_MS,
            confirm_window: DEFAULT_CONFIRM_WINDOW,
            kind: None,
            number: None,
        }
    }
}

impl AgentConfig {
    pub fn wants(&self, event: &str) -> bool {
        self.events.wants(event)
    }
}

/// Parses and checks one agent's block; the daemon prefixes the message
/// with `crews.<c>.agents.<a>.plugins.github: `.
pub fn parse_agent(config: &Value) -> Result<AgentConfig, ConfigError> {
    let c: AgentConfig = deserialize(config)?;
    c.events.validate()?;
    validate_key_delay(c.key_delay_ms)?;
    let missing = |path: &str| ConfigError {
        path: path.into(),
        message: "missing".into(),
    };
    if c.kind.is_none() {
        return Err(missing("kind"));
    }
    match c.number {
        None => return Err(missing("number")),
        Some(0) => {
            return Err(ConfigError {
                path: "number".into(),
                message: "must be at least 1".into(),
            });
        }
        Some(_) => {}
    }
    Ok(c)
}
```

- [ ] **Step 6: Run and commit**

Run: `mise run plugin github`
Expected: PASS (only `config` exists yet; `lib.rs` lists only `config`, `main.rs` can be a stub `fn main() { eprintln!("github: not built yet"); std::process::exit(1) }` until Task 10).

```bash
git add plugins/github
git commit -m "feat(github): the plugin project skeleton and its config (Spec M §3, §4)"
```

---

### Task 2: `GitHubPort` and its fake

**Files:**
- Create: `plugins/github/src/github.rs`.

**Interfaces:**
- Produces:

```rust
pub enum GitHubError { RateLimited { retry_after_ms: u64 }, Auth(String), NotFound, Other(String) }
pub enum Permission { Admin, Maintain, Write, Triage, Read, None }   // Permission::may_prompt(self) -> bool
pub enum Target { Issue(u64), Comment(u64) }                          // where a reaction goes
pub struct ReviewComment { path, side: String, line: Option<u64>, original_line: Option<u64>, diff_hunk: String, body: String }
pub trait GitHubPort: Send + Sync + 'static {
    fn app_slug(&self) -> …Result<String, GitHubError>;
    fn default_branch(&self, installation: u64, repo: &str) -> …Result<String, GitHubError>;
    fn read_file(&self, installation: u64, repo: &str, path: &str, git_ref: &str) -> …Result<Option<String>, GitHubError>;
    fn permission(&self, installation: u64, repo: &str, login: &str) -> …Result<Permission, GitHubError>;
    fn comment(&self, installation: u64, repo: &str, number: u64, body: &str) -> …Result<u64, GitHubError>;
    fn edit_comment(&self, installation: u64, repo: &str, comment_id: u64, body: &str) -> …Result<(), GitHubError>;
    fn react(&self, installation: u64, repo: &str, target: Target, content: &str) -> …Result<(), GitHubError>;
    fn review_comments(&self, installation: u64, repo: &str, number: u64, review_id: u64) -> …Result<Vec<ReviewComment>, GitHubError>;
}
pub mod fake { pub enum Call { … one per method … }; pub struct FakePort; new(slug) / calls() / take_calls() / fail_next(GitHubError) / set_file(repo, path, text) / set_permission(repo, login, Permission) / set_default_branch(repo, branch) / set_review_comments(repo, review_id, Vec<ReviewComment>) / delete_comment(id) }
```

- [ ] **Step 1: Write the module with its tests**

`src/github.rs`:

```rust
//! The GitHub port (M-11): what the actor needs from GitHub, behind a
//! trait with a recording fake, so the ordering rules are unit-tested
//! without GitHub. `client.rs` is the real implementation.

use std::future::Future;

/// Reactions GitHub accepts on issues and comments.
pub const EYES: &str = "eyes";
pub const PLUS_ONE: &str = "+1";
pub const MINUS_ONE: &str = "-1";
pub const CONFUSED: &str = "confused";
pub const HOORAY: &str = "hooray";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitHubError {
    #[error("rate limited, retry in {retry_after_ms} ms")]
    RateLimited { retry_after_ms: u64 },
    #[error("auth: {0}")]
    Auth(String),
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Other(String),
}

/// A collaborator's permission on a repository (`role_name` of
/// `GET /repos/{o}/{r}/collaborators/{login}/permission`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Admin,
    Maintain,
    Write,
    Triage,
    Read,
    None,
}

impl Permission {
    /// M-2: write permission is the boundary.
    pub fn may_prompt(self) -> bool {
        matches!(self, Permission::Admin | Permission::Maintain | Permission::Write)
    }

    /// From `role_name` (exact) or, when absent, `permission` (coarser:
    /// `maintain` reports as `write`, `triage` as `read`).
    pub fn parse(role_name: Option<&str>, permission: Option<&str>) -> Permission {
        match role_name.or(permission).unwrap_or("none") {
            "admin" => Permission::Admin,
            "maintain" => Permission::Maintain,
            "write" => Permission::Write,
            "triage" => Permission::Triage,
            "read" => Permission::Read,
            _ => Permission::None,
        }
    }
}

/// Where a reaction goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The issue or pull request body itself.
    Issue(u64),
    /// A comment, by id.
    Comment(u64),
}

/// One inline comment of a submitted review.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ReviewComment {
    pub path: String,
    #[serde(default)]
    pub side: String,
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default)]
    pub original_line: Option<u64>,
    #[serde(default)]
    pub diff_hunk: String,
    #[serde(default)]
    pub body: String,
}

pub trait GitHubPort: Send + Sync + 'static {
    /// `GET /app` with the App JWT: proves the key and answers the slug
    /// the mention is matched against.
    fn app_slug(&self) -> impl Future<Output = Result<String, GitHubError>> + Send;
    fn default_branch(
        &self,
        installation: u64,
        repo: &str,
    ) -> impl Future<Output = Result<String, GitHubError>> + Send;
    /// The decoded file at `path` on `git_ref`; `None` when absent.
    fn read_file(
        &self,
        installation: u64,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> impl Future<Output = Result<Option<String>, GitHubError>> + Send;
    fn permission(
        &self,
        installation: u64,
        repo: &str,
        login: &str,
    ) -> impl Future<Output = Result<Permission, GitHubError>> + Send;
    /// Posts a comment on issue or PR `number`; answers its id.
    fn comment(
        &self,
        installation: u64,
        repo: &str,
        number: u64,
        body: &str,
    ) -> impl Future<Output = Result<u64, GitHubError>> + Send;
    /// `NotFound` when the comment was deleted.
    fn edit_comment(
        &self,
        installation: u64,
        repo: &str,
        comment_id: u64,
        body: &str,
    ) -> impl Future<Output = Result<(), GitHubError>> + Send;
    fn react(
        &self,
        installation: u64,
        repo: &str,
        target: Target,
        content: &str,
    ) -> impl Future<Output = Result<(), GitHubError>> + Send;
    fn review_comments(
        &self,
        installation: u64,
        repo: &str,
        number: u64,
        review_id: u64,
    ) -> impl Future<Output = Result<Vec<ReviewComment>, GitHubError>> + Send;
}

/// A recording port for tests; always compiled, `tests/plugin_it.rs`
/// uses it.
pub mod fake {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    use super::{GitHubError, GitHubPort, Permission, ReviewComment, Target};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Call {
        AppSlug,
        DefaultBranch { repo: String },
        ReadFile { repo: String, path: String, git_ref: String },
        Permission { repo: String, login: String },
        Comment { repo: String, number: u64, body: String, id: u64 },
        EditComment { repo: String, comment_id: u64, body: String },
        React { repo: String, target: Target, content: String },
        ReviewComments { repo: String, number: u64, review_id: u64 },
    }

    #[derive(Default)]
    struct Inner {
        calls: Vec<Call>,
        next_id: u64,
        fail_next: Option<GitHubError>,
        files: HashMap<(String, String, String), String>,
        permissions: HashMap<(String, String), Permission>,
        default_branches: HashMap<String, String>,
        reviews: HashMap<(String, u64), Vec<ReviewComment>>,
        deleted: HashSet<u64>,
    }

    #[derive(Clone)]
    pub struct FakePort {
        slug: String,
        inner: Arc<Mutex<Inner>>,
    }

    impl FakePort {
        pub fn new(slug: &str) -> Self {
            Self {
                slug: slug.to_string(),
                inner: Arc::new(Mutex::new(Inner {
                    next_id: 1000,
                    ..Inner::default()
                })),
            }
        }
        fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
            self.inner.lock().unwrap_or_else(|e| e.into_inner())
        }
        pub fn calls(&self) -> Vec<Call> {
            self.lock().calls.clone()
        }
        pub fn take_calls(&self) -> Vec<Call> {
            std::mem::take(&mut self.lock().calls)
        }
        /// The next call of any kind fails once with `error`.
        pub fn fail_next(&self, error: GitHubError) {
            self.lock().fail_next = Some(error);
        }
        pub fn set_file(&self, repo: &str, git_ref: &str, path: &str, text: &str) {
            self.lock()
                .files
                .insert((repo.into(), git_ref.into(), path.into()), text.into());
        }
        pub fn set_permission(&self, repo: &str, login: &str, p: Permission) {
            self.lock().permissions.insert((repo.into(), login.into()), p);
        }
        pub fn set_default_branch(&self, repo: &str, branch: &str) {
            self.lock().default_branches.insert(repo.into(), branch.into());
        }
        pub fn set_review_comments(&self, repo: &str, review_id: u64, comments: Vec<ReviewComment>) {
            self.lock().reviews.insert((repo.into(), review_id), comments);
        }
        /// The comment is gone: `edit_comment` answers `NotFound`.
        pub fn delete_comment(&self, id: u64) {
            self.lock().deleted.insert(id);
        }
        fn check(&self) -> Result<(), GitHubError> {
            match self.lock().fail_next.take() {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
        fn record(&self, call: Call) {
            self.lock().calls.push(call);
        }
    }

    impl GitHubPort for FakePort {
        async fn app_slug(&self) -> Result<String, GitHubError> {
            self.check()?;
            self.record(Call::AppSlug);
            Ok(self.slug.clone())
        }
        async fn default_branch(&self, _i: u64, repo: &str) -> Result<String, GitHubError> {
            self.check()?;
            self.record(Call::DefaultBranch { repo: repo.into() });
            Ok(self
                .lock()
                .default_branches
                .get(repo)
                .cloned()
                .unwrap_or_else(|| "main".into()))
        }
        async fn read_file(
            &self,
            _i: u64,
            repo: &str,
            path: &str,
            git_ref: &str,
        ) -> Result<Option<String>, GitHubError> {
            self.check()?;
            self.record(Call::ReadFile {
                repo: repo.into(),
                path: path.into(),
                git_ref: git_ref.into(),
            });
            Ok(self
                .lock()
                .files
                .get(&(repo.into(), git_ref.into(), path.into()))
                .cloned())
        }
        async fn permission(&self, _i: u64, repo: &str, login: &str) -> Result<Permission, GitHubError> {
            self.check()?;
            self.record(Call::Permission {
                repo: repo.into(),
                login: login.into(),
            });
            Ok(self
                .lock()
                .permissions
                .get(&(repo.into(), login.into()))
                .copied()
                .unwrap_or(Permission::None))
        }
        async fn comment(&self, _i: u64, repo: &str, number: u64, body: &str) -> Result<u64, GitHubError> {
            self.check()?;
            let id = {
                let mut g = self.lock();
                g.next_id += 1;
                g.next_id
            };
            self.record(Call::Comment {
                repo: repo.into(),
                number,
                body: body.into(),
                id,
            });
            Ok(id)
        }
        async fn edit_comment(&self, _i: u64, repo: &str, comment_id: u64, body: &str) -> Result<(), GitHubError> {
            self.check()?;
            if self.lock().deleted.contains(&comment_id) {
                return Err(GitHubError::NotFound);
            }
            self.record(Call::EditComment {
                repo: repo.into(),
                comment_id,
                body: body.into(),
            });
            Ok(())
        }
        async fn react(&self, _i: u64, repo: &str, target: Target, content: &str) -> Result<(), GitHubError> {
            self.check()?;
            self.record(Call::React {
                repo: repo.into(),
                target,
                content: content.into(),
            });
            Ok(())
        }
        async fn review_comments(&self, _i: u64, repo: &str, number: u64, review_id: u64) -> Result<Vec<ReviewComment>, GitHubError> {
            self.check()?;
            self.record(Call::ReviewComments {
                repo: repo.into(),
                number,
                review_id,
            });
            Ok(self
                .lock()
                .reviews
                .get(&(repo.into(), review_id))
                .cloned()
                .unwrap_or_default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Call, FakePort};
    use super::*;

    #[test]
    fn permission_prefers_role_name_and_write_is_the_boundary() {
        assert_eq!(Permission::parse(Some("maintain"), Some("write")), Permission::Maintain);
        assert_eq!(Permission::parse(None, Some("write")), Permission::Write);
        assert_eq!(Permission::parse(None, None), Permission::None);
        assert!(Permission::Admin.may_prompt() && Permission::Write.may_prompt());
        assert!(!Permission::Triage.may_prompt() && !Permission::Read.may_prompt());
    }

    #[tokio::test]
    async fn the_fake_records_mints_ids_and_fails_once() {
        let p = FakePort::new("balerix");
        assert_eq!(p.app_slug().await.unwrap(), "balerix");
        let id = p.comment(1, "acme/api", 12, "hi").await.unwrap();
        assert_eq!(id, 1001);
        p.fail_next(GitHubError::Other("boom".into()));
        assert!(p.comment(1, "acme/api", 12, "again").await.is_err());
        assert_eq!(p.comment(1, "acme/api", 12, "again").await.unwrap(), 1002);
        p.delete_comment(1001);
        assert_eq!(p.edit_comment(1, "acme/api", 1001, "x").await, Err(GitHubError::NotFound));
        assert_eq!(p.read_file(1, "acme/api", ".balerix.yaml", "main").await.unwrap(), None);
        p.set_file("acme/api", "main", ".balerix.yaml", "kind: Fleet");
        assert_eq!(
            p.read_file(1, "acme/api", ".balerix.yaml", "main").await.unwrap().as_deref(),
            Some("kind: Fleet")
        );
        assert!(matches!(p.calls()[0], Call::AppSlug));
        assert_eq!(p.calls().len(), 6);
    }
}
```

- [ ] **Step 2: Run, then commit**

Add `pub mod github;` to `lib.rs`. Run: `mise run plugin github`
Expected: PASS.

```bash
git add plugins/github
git commit -m "feat(github): the GitHubPort trait and its recording fake (M-11)"
```

---

### Task 3: `mention.rs` and `repo_config.rs` (pure)

**Files:**
- Create: `plugins/github/src/mention.rs`, `plugins/github/src/repo_config.rs`, `plugins/github/src/snapshots/` (insta writes them).

**Interfaces:**
- Produces:
  - `mention::mentions(body: &str, slug: &str) -> bool`.
  - `mention::fleet_name(repo: &str) -> String` (`gh-` + sanitised `owner/name`, cut to 63).
  - `mention::is_plugin_agent(name: &str) -> bool` (`issue-<n>` / `pr-<n>`).
  - `repo_config::Live { kind: Kind, number: u64, head: Option<String> }`.
  - `repo_config::prepare(text: &str, repo: &str, fleet: &str, default_branch: &str, live: &[Live]) -> Result<Value, String>`: §6's three checks, then `name`, the crew's `ref` default, and one agent per live session injected; the `Err` is the message to post.
  - `repo_config::MAX_FILE_BYTES: usize = 256 * 1024`.

- [ ] **Step 1: `mention.rs` with tests**

```rust
//! Mention detection and the names the plugin builds (Spec M §6, §8.2).

/// `@<slug>`, case-insensitive, at a word boundary, outside fenced and
/// inline code.
pub fn mentions(body: &str, slug: &str) -> bool {
    let needle = format!("@{}", slug.to_ascii_lowercase());
    let mut in_fence = false;
    for line in body.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        // strip inline code spans
        let mut out = String::with_capacity(line.len());
        let mut in_code = false;
        for c in line.chars() {
            if c == '`' {
                in_code = !in_code;
                out.push(' ');
            } else if in_code {
                out.push(' ');
            } else {
                out.push(c);
            }
        }
        let lower = out.to_ascii_lowercase();
        let mut from = 0;
        while let Some(i) = lower[from..].find(&needle) {
            let start = from + i;
            let end = start + needle.len();
            let before_ok = start == 0
                || !lower[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '@');
            let after_ok = !lower[end..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '-' || c == '_');
            if before_ok && after_ok {
                return true;
            }
            from = end;
        }
    }
    false
}

/// `gh-` + `owner/name` lower-cased, every run outside `[a-z0-9]` one
/// `-`, trimmed of `-`, cut to 63 bytes (the daemon's name rule).
pub fn fleet_name(repo: &str) -> String {
    let mut out = String::from("gh-");
    let mut dash = false;
    for c in repo.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-');
    let mut s = trimmed.to_string();
    if s.len() > 63 {
        s.truncate(63);
        s = s.trim_end_matches('-').to_string();
    }
    s
}

/// The names the plugin owns inside a crew (Spec M §6 check 3).
pub fn is_plugin_agent(name: &str) -> bool {
    let n = name
        .strip_prefix("issue-")
        .or_else(|| name.strip_prefix("pr-"));
    n.is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_match_the_slug_at_word_boundaries_case_insensitively() {
        assert!(mentions("@balerix please look", "balerix"));
        assert!(mentions("hey @Balerix, look", "balerix"));
        assert!(mentions("(@balerix)", "balerix"));
        assert!(!mentions("@balerix-bot please", "balerix"));
        assert!(!mentions("email me@balerix", "balerix"));
        assert!(!mentions("nothing here", "balerix"));
    }

    #[test]
    fn mentions_inside_code_are_not_mentions() {
        assert!(!mentions("run `@balerix` here", "balerix"));
        assert!(!mentions("```\n@balerix\n```", "balerix"));
        assert!(!mentions("~~~sh\n@balerix\n~~~", "balerix"));
        assert!(mentions("```\ncode\n```\n@balerix after", "balerix"));
    }

    #[test]
    fn fleet_names_are_sanitised_and_cut() {
        assert_eq!(fleet_name("Acme/Payments"), "gh-acme-payments");
        assert_eq!(fleet_name("acme/my_repo.v2"), "gh-acme-my-repo-v2");
        assert_eq!(fleet_name("a//b--"), "gh-a-b");
        let long = fleet_name(&format!("o/{}", "x".repeat(100)));
        assert_eq!(long.len(), 63);
        assert!(!long.ends_with('-'));
        let cut_on_dash = fleet_name(&format!("o/{}-{}", "x".repeat(58), "y".repeat(10)));
        assert!(cut_on_dash.len() <= 63 && !cut_on_dash.ends_with('-'));
    }

    #[test]
    fn plugin_agent_names_are_issue_and_pr_numbers() {
        assert!(is_plugin_agent("issue-12") && is_plugin_agent("pr-3"));
        assert!(!is_plugin_agent("issue-") && !is_plugin_agent("pr-x") && !is_plugin_agent("alice"));
    }
}
```

- [ ] **Step 2: `repo_config.rs` with tests**

```rust
//! The repository's fleet file (Spec M §6): an ordinary fleet file with
//! one crew naming this repository, read from the default branch, into
//! which the plugin injects its agents. Every refusal is the message
//! posted on the issue.

use serde_json::{Map, Value, json};

use crate::config::Kind;
use crate::mention::is_plugin_agent;

/// Larger files are refused before parsing.
pub const MAX_FILE_BYTES: usize = 256 * 1024;

/// A session the fleet must carry an agent for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Live {
    pub kind: Kind,
    pub number: u64,
    /// The PR's head ref (`branch`, Spec L §6); `None` for an issue.
    pub head: Option<String>,
}

/// §6: check, then set `name`, default the crew's `ref`, add one agent
/// per live session. The result is the `PUT plugin-host/fleets/<name>`
/// body.
pub fn prepare(
    text: &str,
    repo: &str,
    fleet: &str,
    default_branch: &str,
    live: &[Live],
) -> Result<Value, String> {
    if text.len() > MAX_FILE_BYTES {
        return Err(format!(".balerix.yaml: larger than {MAX_FILE_BYTES} bytes"));
    }
    let mut file: Value =
        serde_norway::from_str(text).map_err(|e| format!(".balerix.yaml: invalid YAML: {e}"))?;
    let Some(root) = file.as_object_mut() else {
        return Err(".balerix.yaml: expected a mapping".into());
    };
    if root.get("apiVersion").and_then(Value::as_str) != Some("balerix/v1") {
        return Err(".balerix.yaml: apiVersion: expected \"balerix/v1\"".into());
    }
    if root.get("kind").and_then(Value::as_str) != Some("Fleet") {
        return Err(".balerix.yaml: kind: expected \"Fleet\"".into());
    }
    let crews = match root.get_mut("crews") {
        Some(Value::Object(c)) => c,
        _ => return Err(".balerix.yaml: crews: expected a mapping with one entry".into()),
    };
    if crews.len() != 1 {
        return Err(format!(
            ".balerix.yaml: crews: expected exactly one crew, got {}",
            crews.len()
        ));
    }
    let (crew_name, crew) = match crews.iter_mut().next() {
        Some((k, Value::Object(c))) => (k.clone(), c),
        Some((k, _)) => return Err(format!(".balerix.yaml: crews.{k}: expected a mapping")),
        None => unreachable!("len checked"),
    };
    let declared = crew.get("repo").and_then(Value::as_str).unwrap_or("");
    if !names_repo(declared, repo) {
        return Err(format!(
            ".balerix.yaml: crews.{crew_name}.repo: expected {repo} (this repository), got {declared:?}"
        ));
    }
    if !crew.contains_key("ref") {
        crew.insert("ref".into(), json!(default_branch));
    }
    let agents = match crew.entry("agents").or_insert_with(|| json!({})) {
        Value::Object(a) => a,
        _ => return Err(format!(".balerix.yaml: crews.{crew_name}.agents: expected a mapping")),
    };
    if let Some(taken) = agents.keys().find(|k| is_plugin_agent(k)) {
        return Err(format!(
            ".balerix.yaml: crews.{crew_name}.agents.{taken}: issue-<n> and pr-<n> are the plugin's names"
        ));
    }
    for s in live {
        let mut agent = Map::new();
        agent.insert(
            "plugins".into(),
            json!({ "github": { "kind": s.kind, "number": s.number } }),
        );
        if let Some(head) = &s.head {
            agent.insert("branch".into(), json!(head));
        }
        agents.insert(s.kind.agent_name(s.number), Value::Object(agent));
    }
    root.insert("name".into(), json!(fleet));
    Ok(file)
}

/// `owner/name` case-insensitively, or a clone URL of it.
fn names_repo(declared: &str, repo: &str) -> bool {
    let d = declared.trim().trim_end_matches('/');
    let d = d.strip_suffix(".git").unwrap_or(d);
    let tail = d
        .rsplit_once(':')
        .map(|(_, t)| t)
        .filter(|_| d.starts_with("git@"))
        .unwrap_or_else(|| d.strip_prefix("https://github.com/").unwrap_or(d));
    tail.eq_ignore_ascii_case(repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "apiVersion: balerix/v1\nkind: Fleet\ndefaults:\n  tools: { node: \"22.11.0\" }\ncrews:\n  repo:\n    repo: Acme/Payments\n    agents:\n      reviewer: { claude: { settings: { model: opus } } }\n";

    fn issue(n: u64) -> Live {
        Live { kind: Kind::Issue, number: n, head: None }
    }

    #[test]
    fn a_good_file_gains_the_name_the_ref_and_the_agents() {
        let v = prepare(FILE, "acme/payments", "gh-acme-payments", "develop", &[
            issue(12),
            Live { kind: Kind::Pr, number: 34, head: Some("feature/x".into()) },
        ])
        .unwrap();
        insta::assert_json_snapshot!(v);
        assert_eq!(v["name"], "gh-acme-payments");
        assert_eq!(v["crews"]["repo"]["ref"], "develop");
        assert_eq!(v["crews"]["repo"]["agents"]["pr-34"]["branch"], "feature/x");
        assert_eq!(v["crews"]["repo"]["agents"]["issue-12"]["plugins"]["github"]["number"], 12);
        assert_eq!(v["crews"]["repo"]["agents"]["reviewer"]["claude"]["settings"]["model"], "opus");
    }

    #[test]
    fn injection_leaves_the_rest_of_the_file_intact() {
        let before: Value = serde_norway::from_str(FILE).unwrap();
        let mut after = prepare(FILE, "acme/payments", "gh-acme-payments", "main", &[]).unwrap();
        after.as_object_mut().unwrap().remove("name");
        after["crews"]["repo"].as_object_mut().unwrap().remove("ref");
        assert_eq!(after, before);
    }

    #[test]
    fn an_explicit_ref_is_kept() {
        let f = FILE.replace("    repo: Acme/Payments\n", "    repo: acme/payments\n    ref: release\n");
        let v = prepare(&f, "acme/payments", "f", "main", &[]).unwrap();
        assert_eq!(v["crews"]["repo"]["ref"], "release");
    }

    #[test]
    fn every_refusal_names_what_is_wrong() {
        let cases: [(&str, &str); 7] = [
            ("apiVersion: balerix/v2\nkind: Fleet\ncrews: {}\n", ".balerix.yaml: apiVersion: expected \"balerix/v1\""),
            ("apiVersion: balerix/v1\nkind: Crew\ncrews: {}\n", ".balerix.yaml: kind: expected \"Fleet\""),
            ("apiVersion: balerix/v1\nkind: Fleet\n", ".balerix.yaml: crews: expected a mapping with one entry"),
            ("apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  a: { repo: acme/payments }\n  b: { repo: acme/payments }\n", ".balerix.yaml: crews: expected exactly one crew, got 2"),
            ("apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  repo: { repo: other/thing }\n", ".balerix.yaml: crews.repo.repo: expected acme/payments (this repository), got \"other/thing\""),
            ("apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  repo:\n    repo: acme/payments\n    agents: { issue-7: {} }\n", ".balerix.yaml: crews.repo.agents.issue-7: issue-<n> and pr-<n> are the plugin's names"),
            ("not: [valid", ".balerix.yaml: invalid YAML: "),
        ];
        for (text, want) in cases {
            let e = prepare(text, "acme/payments", "f", "main", &[]).unwrap_err();
            assert!(e.starts_with(want), "{text:?}: {e}");
        }
        let big = format!("apiVersion: balerix/v1\nkind: Fleet\n# {}\n", "x".repeat(MAX_FILE_BYTES));
        assert!(prepare(&big, "acme/payments", "f", "main", &[]).unwrap_err().contains("larger than"));
    }

    #[test]
    fn the_repo_may_be_a_clone_url_or_differ_in_case() {
        for declared in ["acme/payments", "ACME/payments", "https://github.com/acme/payments.git", "git@github.com:acme/payments.git"] {
            let f = FILE.replace("Acme/Payments", declared);
            assert!(prepare(&f, "acme/payments", "f", "main", &[]).is_ok(), "{declared}");
        }
        assert!(!names_repo("acme/payments-v2", "acme/payments"));
    }
}
```

- [ ] **Step 3: Run, review the snapshot, commit**

Add `pub mod mention;` and `pub mod repo_config;` to `lib.rs`. Run: `cd plugins/github && mise x -- cargo nextest run mention repo_config`, read `src/snapshots/*.snap.new` (the injected file must carry the two agents and `reviewer` unchanged), then `mise x -- cargo insta accept`. Run `mise run plugin github`.
Expected: PASS.

```bash
git add plugins/github
git commit -m "feat(github): mention detection, fleet names and .balerix.yaml injection (Spec M §6)"
```

---

### Task 4: `prompt.rs` and `status.rs` (pure)

**Files:**
- Create: `plugins/github/src/prompt.rs`, `plugins/github/src/status.rs`.

**Interfaces:**
- Produces:
  - `prompt::Start<'a> { repo, kind: Kind, number: u64, title, url, body, branch, base: Option<&str>, asker, comment: Option<&str>, resumed: bool }`, `prompt::first(&Start) -> String`.
  - `prompt::review(reviewer: &str, state: &str, commit: &str, body: &str, comments: &[ReviewComment], base: &str) -> Option<String>` (`None` when neither body nor comments).
  - `status::MAX_LINES: usize = 20`, `status::Status { agent: String, session: Option<String>, phase: String }` with `push(&mut self, at: u64 /*unix secs*/, text: &str)` and `render(&self) -> String`.

- [ ] **Step 1: `prompt.rs` with snapshots**

```rust
//! What the agent is told (Spec M §8.2, §9): the first prompt, and a
//! submitted review as one message.

use balerix_plugin_common::review::{self, Comment, Review, Side};

use crate::config::Kind;
use crate::github::ReviewComment;

/// The facts the first prompt renders.
#[derive(Debug, Clone)]
pub struct Start<'a> {
    pub repo: &'a str,
    pub kind: Kind,
    pub number: u64,
    pub title: &'a str,
    pub url: &'a str,
    pub body: &'a str,
    /// The agent's branch: the PR head, or `balerix/<fleet>/<crew>/<agent>`.
    pub branch: &'a str,
    /// What the branch is taken from (the crew ref) or merged into (the PR base).
    pub base: &'a str,
    pub asker: &'a str,
    /// The mentioning comment, when the mention was not the body itself.
    pub comment: Option<&'a str>,
    /// A row existed before (an idle stop): point at the earlier work.
    pub resumed: bool,
}

/// Spec M §8.2.
pub fn first(s: &Start<'_>) -> String {
    let what = match s.kind {
        Kind::Issue => "issue",
        Kind::Pr => "pull request",
    };
    let branch = match s.kind {
        Kind::Issue => format!("Branch: {} (from {})", s.branch, s.base),
        Kind::Pr => format!("Branch: {} (into {})", s.branch, s.base),
    };
    let mut out = format!(
        "You are attached to {} {what} #{}: {}\n{}\n\n{branch}\n",
        s.repo,
        s.number,
        s.title.trim(),
        s.url
    );
    if s.resumed {
        out.push_str(&format!(
            "\nEarlier work on this {what} is on branch {}; continue from it.\n",
            s.branch
        ));
    }
    out.push('\n');
    out.push_str(s.body.trim());
    out.push('\n');
    if let Some(c) = s.comment {
        out.push_str(&format!("\n---\n@{} asked:\n{}\n", s.asker, c.trim()));
    }
    out
}

/// Spec M §9: `Review by @bob: changes requested, at 3f9c2a1` over
/// common's rendering. `None` when there is nothing to deliver.
pub fn review(
    reviewer: &str,
    state: &str,
    commit: &str,
    body: &str,
    comments: &[ReviewComment],
    base: &str,
) -> Option<String> {
    if body.trim().is_empty() && comments.is_empty() {
        return None;
    }
    let verdict = match state.to_ascii_lowercase().as_str() {
        "approved" => "approved",
        "changes_requested" => "changes requested",
        "commented" => "commented",
        other => other,
    }
    .to_string();
    let short = commit.get(..7).unwrap_or(commit);
    let r = Review {
        head: short.to_string(),
        base_ref: base.to_string(),
        summary: body.trim().to_string(),
        comments: comments
            .iter()
            .map(|c| Comment {
                path: c.path.clone(),
                side: if c.side.eq_ignore_ascii_case("LEFT") { Side::Old } else { Side::New },
                line: c.line.or(c.original_line).unwrap_or(0),
                text: c.diff_hunk.lines().last().unwrap_or("").to_string(),
                body: c.body.clone(),
            })
            .collect(),
    };
    if let Err(e) = review::validate(&r) {
        return Some(format!("Review by @{reviewer}: {verdict}, at {short}\n(not rendered: {e})"));
    }
    Some(format!(
        "Review by @{reviewer}: {verdict}, at {short}\n{}",
        review::render_message(&r)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start() -> Start<'static> {
        Start {
            repo: "acme/payments",
            kind: Kind::Issue,
            number: 12,
            title: "Refunds double-post ",
            url: "https://github.com/acme/payments/issues/12",
            body: "When a refund is retried it posts twice.\n\nSteps: …",
            branch: "balerix/gh-acme-payments/repo/issue-12",
            base: "main",
            asker: "alice",
            comment: Some("@balerix please fix this"),
            resumed: false,
        }
    }

    #[test]
    fn the_first_prompt_for_an_issue() {
        insta::assert_snapshot!(first(&start()));
    }

    #[test]
    fn the_first_prompt_for_a_pr_and_a_resume() {
        let s = Start {
            kind: Kind::Pr,
            number: 34,
            branch: "feature/refunds",
            base: "main",
            comment: None,
            resumed: true,
            ..start()
        };
        insta::assert_snapshot!(first(&s));
    }

    #[test]
    fn a_review_renders_once_with_the_header_and_skips_an_empty_one() {
        assert_eq!(review("bob", "APPROVED", "3f9c2a1deadbeef", "  ", &[], "main"), None);
        let comments = vec![ReviewComment {
            path: "src/lib.rs".into(),
            side: "RIGHT".into(),
            line: Some(42),
            original_line: None,
            diff_hunk: "@@ -1 +1 @@\n+    let x = foo();".into(),
            body: "This unwrap can panic.".into(),
        }];
        let m = review("bob", "changes_requested", "3f9c2a1deadbeef", "Close.", &comments, "main").unwrap();
        insta::assert_snapshot!(m);
        assert!(m.starts_with("Review by @bob: changes requested, at 3f9c2a1\n"));
        assert!(m.contains("src/lib.rs line 42 (new)"));
        assert!(m.contains("> +    let x = foo();"));
    }
}
```

- [ ] **Step 2: `status.rs` with a snapshot**

```rust
//! The status comment (Spec M §8.3): one comment per session, edited in
//! place, twenty lines at most.

use std::collections::VecDeque;

use balerix_plugin_common::render::short_session;

/// Lines kept; older ones fall off the top.
pub const MAX_LINES: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub agent: String,
    pub session: Option<String>,
    pub phase: String,
    lines: VecDeque<(u64, String)>,
}

impl Status {
    pub fn new(agent: &str) -> Self {
        Self {
            agent: agent.to_string(),
            session: None,
            phase: "starting".into(),
            lines: VecDeque::new(),
        }
    }

    /// Adds a line stamped `at` (unix seconds, rendered as UTC hh:mm),
    /// its first line only.
    pub fn push(&mut self, at: u64, text: &str) {
        let first = text.lines().next().unwrap_or("").trim().to_string();
        self.lines.push_back((at, first));
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
        }
    }

    pub fn render(&self) -> String {
        let session = self
            .session
            .as_deref()
            .map(|s| format!(" · session `{}`", short_session(s)))
            .unwrap_or_default();
        let mut out = format!(
            "**balerix** · `{}`{session} · **{}**\n",
            self.agent, self.phase
        );
        for (at, line) in &self.lines {
            out.push_str(&format!("\n- {} {line}", hhmm(*at)));
        }
        out
    }
}

fn hhmm(secs: u64) -> String {
    format!("{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_comment_keeps_twenty_lines_and_names_the_phase() {
        let mut s = Status::new("gh-acme-payments/repo/issue-12");
        s.session = Some("0199aa11-2222-3333".into());
        s.phase = "ready".into();
        s.push(14 * 3600 + 2 * 60, "session started (startup)\nsecond line ignored");
        s.push(14 * 3600 + 5 * 60, "needs you: Claude needs your permission to use Bash");
        insta::assert_snapshot!(s.render());
        for i in 0..25 {
            s.push(i * 60, &format!("line {i}"));
        }
        let r = s.render();
        assert_eq!(r.matches("\n- ").count(), MAX_LINES);
        assert!(!r.contains("session started"), "the oldest fell off");
        assert!(r.ends_with("- 00:24 line 24"));
    }
}
```

- [ ] **Step 3: Run, review the snapshots, commit**

Add `pub mod prompt;` and `pub mod status;` to `lib.rs`. Run: `cd plugins/github && mise x -- cargo nextest run prompt status`; read each `.snap.new` against Spec M §8.2, §8.3 and §9 (the first prompt's exact lines, the header `**balerix** · \`…\` · session \`0199aa11\` · **ready**`), then `mise x -- cargo insta accept`. Run `mise run plugin github`.
Expected: PASS.

```bash
git add plugins/github
git commit -m "feat(github): the first prompt, the review message and the status comment (Spec M §8.2, §8.3, §9)"
```

---

### Task 5: `webhook.rs`, the listener

**Files:**
- Create: `plugins/github/src/webhook.rs`.

**Interfaces:**
- Consumes: `balerix_plugin_sdk::auth::constant_time_eq`, common's `Queue`, `Secret`.
- Produces:
  - `webhook::Author { login: String, bot: bool }`.
  - `webhook::WebhookEvent` (the spec's five variants; `PrOpened.head_repo: String`, `Comment.is_pr: bool`, `Closed.merged: bool`, `ReviewSubmitted { state, body, commit, review_id }`).
  - `webhook::verify(secret: &Secret, body: &[u8], signature_header: Option<&str>) -> bool`.
  - `webhook::parse(event: &str, payload: &Value) -> Option<WebhookEvent>`.
  - `webhook::Listener::new(secret: Secret, sink: impl Fn(WebhookEvent) + Send + Sync + 'static, counters: IntCounterVec /*webhooks_total{event,outcome}*/) -> Listener`, `Listener::router(self: Arc<Self>) -> axum::Router`, `webhook::serve(listener: tokio::net::TcpListener, router: Router) -> impl Future`.
  - `webhook::MAX_BODY: usize = 1 << 20`, `webhook::RING: usize = 4096`.

- [ ] **Step 1: The module with pure tests**

```rust
//! Ingress (Spec M §7): `POST /webhook` on the plugin's own listener,
//! HMAC-verified, deduplicated by delivery id, parsed into
//! `WebhookEvent` and handed to the actor. Answers 202 and never waits
//! on anything (G-11).

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use balerix_plugin_common::config::Secret;
use balerix_plugin_sdk::auth::constant_time_eq;
use balerix_plugin_sdk::metrics::IntCounterVec;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

/// Bodies beyond this are 413.
pub const MAX_BODY: usize = 1 << 20;
/// Delivery ids remembered.
pub const RING: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    pub login: String,
    /// `user.type == "Bot"`.
    pub bot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookEvent {
    IssueOpened { repo: String, installation: u64, number: u64, author: Author, title: String, body: String, url: String },
    PrOpened { repo: String, installation: u64, number: u64, author: Author, title: String, body: String, url: String, head: String, head_repo: String, base: String },
    Comment { repo: String, installation: u64, number: u64, author: Author, comment_id: u64, body: String, is_pr: bool },
    Closed { repo: String, installation: u64, number: u64, merged: bool },
    ReviewSubmitted { repo: String, installation: u64, number: u64, author: Author, review_id: u64, state: String, body: String, commit: String },
}

impl WebhookEvent {
    pub fn repo(&self) -> &str {
        match self {
            Self::IssueOpened { repo, .. } | Self::PrOpened { repo, .. } | Self::Comment { repo, .. } | Self::Closed { repo, .. } | Self::ReviewSubmitted { repo, .. } => repo,
        }
    }
    pub fn number(&self) -> u64 {
        match self {
            Self::IssueOpened { number, .. } | Self::PrOpened { number, .. } | Self::Comment { number, .. } | Self::Closed { number, .. } | Self::ReviewSubmitted { number, .. } => *number,
        }
    }
    pub fn installation(&self) -> u64 {
        match self {
            Self::IssueOpened { installation, .. } | Self::PrOpened { installation, .. } | Self::Comment { installation, .. } | Self::Closed { installation, .. } | Self::ReviewSubmitted { installation, .. } => *installation,
        }
    }
}

/// `X-Hub-Signature-256: sha256=<hex>` over the raw body, compared in
/// constant time.
pub fn verify(secret: &Secret, body: &[u8], header: Option<&str>) -> bool {
    let Some(hex) = header.and_then(|h| h.strip_prefix("sha256=")) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.expose().as_bytes()) else {
        return false;
    };
    mac.update(body);
    let ours: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    constant_time_eq(ours.as_bytes(), hex.trim().as_bytes())
}

fn s(v: &Value, ptr: &str) -> String {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or("").to_string()
}
fn n(v: &Value, ptr: &str) -> Option<u64> {
    v.pointer(ptr).and_then(Value::as_u64)
}
fn author(v: &Value, ptr: &str) -> Author {
    Author {
        login: s(v, &format!("{ptr}/login")),
        bot: v.pointer(&format!("{ptr}/type")).and_then(Value::as_str) == Some("Bot"),
    }
}

/// The events the plugin reads; everything else is `None`.
pub fn parse(event: &str, p: &Value) -> Option<WebhookEvent> {
    let repo = s(p, "/repository/full_name");
    let installation = n(p, "/installation/id")?;
    if repo.is_empty() {
        return None;
    }
    let action = s(p, "/action");
    match (event, action.as_str()) {
        ("issues", "opened") => Some(WebhookEvent::IssueOpened {
            repo, installation,
            number: n(p, "/issue/number")?,
            author: author(p, "/issue/user"),
            title: s(p, "/issue/title"),
            body: s(p, "/issue/body"),
            url: s(p, "/issue/html_url"),
        }),
        ("issues", "closed") => Some(WebhookEvent::Closed { repo, installation, number: n(p, "/issue/number")?, merged: false }),
        ("pull_request", "opened") => Some(WebhookEvent::PrOpened {
            repo, installation,
            number: n(p, "/pull_request/number")?,
            author: author(p, "/pull_request/user"),
            title: s(p, "/pull_request/title"),
            body: s(p, "/pull_request/body"),
            url: s(p, "/pull_request/html_url"),
            head: s(p, "/pull_request/head/ref"),
            head_repo: s(p, "/pull_request/head/repo/full_name"),
            base: s(p, "/pull_request/base/ref"),
        }),
        ("pull_request", "closed") => Some(WebhookEvent::Closed {
            repo, installation,
            number: n(p, "/pull_request/number")?,
            merged: p.pointer("/pull_request/merged").and_then(Value::as_bool).unwrap_or(false),
        }),
        ("issue_comment", "created") => Some(WebhookEvent::Comment {
            repo, installation,
            number: n(p, "/issue/number")?,
            author: author(p, "/comment/user"),
            comment_id: n(p, "/comment/id")?,
            body: s(p, "/comment/body"),
            is_pr: p.pointer("/issue/pull_request").is_some(),
        }),
        ("pull_request_review", "submitted") => Some(WebhookEvent::ReviewSubmitted {
            repo, installation,
            number: n(p, "/pull_request/number")?,
            author: author(p, "/review/user"),
            review_id: n(p, "/review/id")?,
            state: s(p, "/review/state"),
            body: s(p, "/review/body"),
            commit: s(p, "/review/commit_id"),
        }),
        _ => None,
    }
}

/// The listener's state.
pub struct Listener {
    secret: Secret,
    sink: Box<dyn Fn(WebhookEvent) + Send + Sync>,
    counters: IntCounterVec,
    seen: Mutex<(VecDeque<String>, HashSet<String>)>,
}

impl Listener {
    pub fn new(secret: Secret, sink: impl Fn(WebhookEvent) + Send + Sync + 'static, counters: IntCounterVec) -> Arc<Self> {
        Arc::new(Self { secret, sink: Box::new(sink), counters, seen: Mutex::new((VecDeque::new(), HashSet::new())) })
    }

    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/webhook", post(deliver))
            .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY))
            .with_state(self)
    }

    fn count(&self, event: &str, outcome: &str) {
        self.counters.with_label_values(&[event, outcome]).inc();
    }

    /// `true` when `id` was already seen; remembers it otherwise.
    fn replayed(&self, id: &str) -> bool {
        let mut g = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if g.1.contains(id) {
            return true;
        }
        g.0.push_back(id.to_string());
        g.1.insert(id.to_string());
        while g.0.len() > RING {
            if let Some(old) = g.0.pop_front() {
                g.1.remove(&old);
            }
        }
        false
    }
}

async fn deliver(State(l): State<Arc<Listener>>, headers: HeaderMap, body: Bytes) -> Response {
    let header = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
    let event = header("x-github-event").unwrap_or_default();
    if !verify(&l.secret, &body, header("x-hub-signature-256").as_deref()) {
        l.count(&event, "bad_signature");
        return (StatusCode::UNAUTHORIZED, "bad signature").into_response();
    }
    if event == "ping" {
        l.count(&event, "handled");
        return StatusCode::OK.into_response();
    }
    if let Some(id) = header("x-github-delivery") && l.replayed(&id) {
        l.count(&event, "duplicate");
        return StatusCode::ACCEPTED.into_response();
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        l.count(&event, "ignored");
        return StatusCode::ACCEPTED.into_response();
    };
    match parse(&event, &payload) {
        Some(ev) => {
            l.count(&event, "handled");
            (l.sink)(ev);
        }
        None => l.count(&event, "ignored"),
    }
    StatusCode::ACCEPTED.into_response()
}

/// Serves `router` on `listener` until the task is dropped.
pub async fn serve(listener: tokio::net::TcpListener, router: Router) -> std::io::Result<()> {
    axum::serve(listener, router).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>())
    }

    #[test]
    fn signatures_verify_in_constant_time_and_need_the_prefix() {
        let secret = Secret::new("s3cret");
        let body = b"{\"x\":1}";
        let good = sign("s3cret", body);
        assert!(verify(&secret, body, Some(&good)));
        assert!(!verify(&secret, body, Some(good.trim_start_matches("sha256="))));
        assert!(!verify(&secret, b"{\"x\":2}", Some(&good)));
        assert!(!verify(&secret, body, Some(&sign("other", body))));
        assert!(!verify(&secret, body, None));
    }

    fn base(repo: &str) -> Value {
        json!({ "repository": { "full_name": repo }, "installation": { "id": 77 } })
    }

    #[test]
    fn the_five_events_parse_and_the_rest_are_none() {
        let mut p = base("acme/api");
        p["action"] = json!("created");
        p["issue"] = json!({ "number": 12, "pull_request": { "url": "x" } });
        p["comment"] = json!({ "id": 5, "body": "@balerix go", "user": { "login": "alice", "type": "User" } });
        assert_eq!(
            parse("issue_comment", &p),
            Some(WebhookEvent::Comment { repo: "acme/api".into(), installation: 77, number: 12, author: Author { login: "alice".into(), bot: false }, comment_id: 5, body: "@balerix go".into(), is_pr: true })
        );
        let mut p = base("acme/api");
        p["action"] = json!("opened");
        p["pull_request"] = json!({ "number": 34, "title": "t", "body": null, "html_url": "u", "user": { "login": "bot[bot]", "type": "Bot" }, "head": { "ref": "feature/x", "repo": { "full_name": "fork/api" } }, "base": { "ref": "main" } });
        match parse("pull_request", &p).unwrap() {
            WebhookEvent::PrOpened { head, head_repo, base, author, body, .. } => {
                assert_eq!((head.as_str(), head_repo.as_str(), base.as_str()), ("feature/x", "fork/api", "main"));
                assert!(author.bot);
                assert_eq!(body, "");
            }
            other => panic!("{other:?}"),
        }
        let mut p = base("acme/api");
        p["action"] = json!("closed");
        p["pull_request"] = json!({ "number": 34, "merged": true });
        assert_eq!(parse("pull_request", &p), Some(WebhookEvent::Closed { repo: "acme/api".into(), installation: 77, number: 34, merged: true }));
        let mut p = base("acme/api");
        p["action"] = json!("submitted");
        p["pull_request"] = json!({ "number": 34 });
        p["review"] = json!({ "id": 9, "state": "changes_requested", "body": "close", "commit_id": "3f9c2a1dead", "user": { "login": "bob", "type": "User" } });
        assert!(matches!(parse("pull_request_review", &p), Some(WebhookEvent::ReviewSubmitted { review_id: 9, .. })));
        let mut p = base("acme/api");
        p["action"] = json!("reopened");
        p["issue"] = json!({ "number": 1 });
        assert_eq!(parse("issues", &p), None);
        assert_eq!(parse("pull_request_review_comment", &base("acme/api")), None);
        assert_eq!(parse("issues", &json!({ "action": "opened", "issue": { "number": 1 } })), None, "no installation");
    }

    fn started(secret: &str) -> (Arc<Listener>, std::sync::mpsc::Receiver<WebhookEvent>, IntCounterVec) {
        let (tx, rx) = std::sync::mpsc::channel();
        let metrics = balerix_plugin_sdk::Metrics::new("github");
        let counters = metrics.int_counter_vec("webhooks_total", "t", &["event", "outcome"]).unwrap();
        let l = Listener::new(Secret::new(secret), move |e| { let _ = tx.send(e); }, counters.clone());
        (l, rx, counters)
    }

    #[tokio::test]
    async fn the_listener_verifies_dedupes_and_answers_202() {
        let (l, rx, counters) = started("s3cret");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, l.router()));
        let c = reqwest::Client::new();
        let url = format!("http://{addr}/webhook");
        let mut p = base("acme/api");
        p["action"] = json!("created");
        p["issue"] = json!({ "number": 12 });
        p["comment"] = json!({ "id": 5, "body": "hi", "user": { "login": "alice", "type": "User" } });
        let body = serde_json::to_vec(&p).unwrap();
        let post = |sig: String, delivery: &str, event: &str, body: Vec<u8>| {
            c.post(&url).header("X-Hub-Signature-256", sig).header("X-GitHub-Delivery", delivery.to_string()).header("X-GitHub-Event", event.to_string()).body(body).send()
        };
        assert_eq!(post(sign("nope", &body), "d1", "issue_comment", body.clone()).await.unwrap().status(), 401);
        assert_eq!(post(sign("s3cret", &body), "d1", "issue_comment", body.clone()).await.unwrap().status(), 202);
        assert!(matches!(rx.recv().unwrap(), WebhookEvent::Comment { comment_id: 5, .. }));
        assert_eq!(post(sign("s3cret", &body), "d1", "issue_comment", body.clone()).await.unwrap().status(), 202, "a replayed delivery");
        assert!(rx.try_recv().is_err(), "enqueued nothing");
        assert_eq!(post(sign("s3cret", b"{}"), "d2", "ping", b"{}".to_vec()).await.unwrap().status(), 200);
        assert_eq!(post(sign("s3cret", &body), "d3", "star", body.clone()).await.unwrap().status(), 202);
        let big = vec![b' '; MAX_BODY + 1];
        assert_eq!(post(sign("s3cret", &big), "d4", "issue_comment", big).await.unwrap().status(), 413);
        let get = |event: &str, outcome: &str| counters.with_label_values(&[event, outcome]).get();
        assert_eq!(get("issue_comment", "bad_signature"), 1);
        assert_eq!(get("issue_comment", "handled"), 1);
        assert_eq!(get("issue_comment", "duplicate"), 1);
        assert_eq!(get("ping", "handled"), 1);
        assert_eq!(get("star", "ignored"), 1);
    }

    #[test]
    fn a_replayed_delivery_is_counted_and_dropped_after_the_ring_wraps() {
        let (l, _rx, _c) = started("s");
        assert!(!l.replayed("a"));
        assert!(l.replayed("a"));
        for i in 0..RING {
            l.replayed(&format!("id{i}"));
        }
        assert!(!l.replayed("a"), "fell off the ring");
    }
}
```

The `413` counter (`too_large`) is axum's body limit answering before the handler; it is not counted (the handler never runs). Record that in §18; the metric's `too_large` label from §11 is dropped.

- [ ] **Step 2: Run and commit**

Add `pub mod webhook;` to `lib.rs`. Run: `mise run plugin github`
Expected: PASS.

```bash
git add plugins/github
git commit -m "feat(github): the webhook listener, HMAC-verified and deduplicated (Spec M §7)"
```

---

### Task 6: `session.rs`, the rows in KV

**Files:**
- Create: `plugins/github/src/session.rs`.

**Interfaces:**
- Consumes: `Host::{kv_get, kv_put, kv_delete, kv_list}`.
- Produces:
  - `session::Session { repo, installation, kind: Kind, number, head: Option<String>, status_comment: Option<u64>, session_id: Option<String>, last_activity: u64, closed: bool }` (serde, all fields `default` on read except `repo`, `installation`, `kind`, `number`).
  - `session::Sessions`: `load(&Host) -> Result<Self, SdkError>`, `get(&self, agent) -> Option<&Session>`, `get_mut`, `by_number(&self, repo, number) -> Option<&str>`, `set(&mut self, &Host, agent, Session) -> Result<(), SdkError>`, `remove(&mut self, &Host, agent) -> Result<(), SdkError>`, `live_in(&self, fleet) -> Vec<repo_config::Live>` (rows with `!closed` under that fleet), `agents_in(&self, fleet) -> Vec<String>`, `open_count(&self) -> usize`.
  - `session::repo_of_fleet(&Host, fleet) -> Result<Option<String>, SdkError>`, `session::set_repo_of_fleet(&Host, fleet, repo) -> Result<(), SdkError>`.
  - `session::session_key(agent)`, `session::repo_key(fleet)`, `session::fleet_of(agent) -> &str` (`fleet/crew/agent` → `fleet`).

- [ ] **Step 1: The module with `FakeHost` tests**

```rust
//! State (Spec M §5): one KV row per agent under `session/<agent>`,
//! mirrored like matrix's maps (store first, then memory), with the
//! reverse map `(repo, number) → agent` derived at load; and
//! `repo/<fleet>`, which repository a sanitised fleet name stands for.

use std::collections::HashMap;

use balerix_plugin_sdk::{Host, SdkError};
use serde::{Deserialize, Serialize};

use crate::config::Kind;
use crate::repo_config::Live;

pub const SESSION_PREFIX: &str = "session/";
pub const REPO_PREFIX: &str = "repo/";

pub fn session_key(agent: &str) -> String {
    format!("{SESSION_PREFIX}{agent}")
}
pub fn repo_key(fleet: &str) -> String {
    format!("{REPO_PREFIX}{fleet}")
}
/// `fleet/crew/agent` → `fleet`.
pub fn fleet_of(agent: &str) -> &str {
    agent.split('/').next().unwrap_or(agent)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub repo: String,
    pub installation: u64,
    pub kind: Kind,
    pub number: u64,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub status_comment: Option<u64>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub last_activity: u64,
    #[serde(default)]
    pub closed: bool,
}

#[derive(Debug, Default)]
pub struct Sessions {
    rows: HashMap<String, Session>,
    by_number: HashMap<(String, u64), String>,
}

impl Sessions {
    pub async fn load(host: &Host) -> Result<Self, SdkError> {
        let mut s = Self::default();
        for key in host.kv_list(SESSION_PREFIX).await? {
            let Some(agent) = key.strip_prefix(SESSION_PREFIX) else { continue };
            let Some(bytes) = host.kv_get(&key).await? else { continue };
            match serde_json::from_slice::<Session>(&bytes) {
                Ok(row) => s.insert(agent.to_string(), row),
                Err(e) => tracing::warn!("github: bad session record {key}: {e}"),
            }
        }
        Ok(s)
    }

    fn insert(&mut self, agent: String, row: Session) {
        if let Some(old) = self.rows.get(&agent) {
            self.by_number.remove(&(old.repo.clone(), old.number));
        }
        self.by_number.insert((row.repo.clone(), row.number), agent.clone());
        self.rows.insert(agent, row);
    }

    pub fn get(&self, agent: &str) -> Option<&Session> {
        self.rows.get(agent)
    }
    pub fn get_mut(&mut self, agent: &str) -> Option<&mut Session> {
        self.rows.get_mut(agent)
    }
    pub fn by_number(&self, repo: &str, number: u64) -> Option<&str> {
        self.by_number.get(&(repo.to_string(), number)).map(String::as_str)
    }

    /// Store first, then memory (J-8's rule): a row the store refused is
    /// a row a restart would not know.
    pub async fn set(&mut self, host: &Host, agent: &str, row: Session) -> Result<(), SdkError> {
        let bytes = serde_json::to_vec(&row).map_err(|e| SdkError::Transport(format!("encode session: {e}")))?;
        host.kv_put(&session_key(agent), &bytes, false).await?;
        self.insert(agent.to_string(), row);
        Ok(())
    }

    pub async fn remove(&mut self, host: &Host, agent: &str) -> Result<(), SdkError> {
        host.kv_delete(&session_key(agent)).await?;
        if let Some(old) = self.rows.remove(agent) {
            self.by_number.remove(&(old.repo, old.number));
        }
        Ok(())
    }

    /// The agents the fleet file must carry: every open row in `fleet`.
    pub fn live_in(&self, fleet: &str) -> Vec<Live> {
        let mut v: Vec<(&String, &Session)> = self.rows.iter().filter(|(a, r)| fleet_of(a) == fleet && !r.closed).collect();
        v.sort_by(|a, b| a.0.cmp(b.0));
        v.into_iter().map(|(_, r)| Live { kind: r.kind, number: r.number, head: r.head.clone() }).collect()
    }

    pub fn agents_in(&self, fleet: &str) -> Vec<String> {
        let mut v: Vec<String> = self.rows.keys().filter(|a| fleet_of(a) == fleet).cloned().collect();
        v.sort();
        v
    }

    pub fn open_count(&self) -> usize {
        self.rows.values().filter(|r| !r.closed).count()
    }

    pub fn agents(&self) -> Vec<String> {
        self.rows.keys().cloned().collect()
    }
}

pub async fn repo_of_fleet(host: &Host, fleet: &str) -> Result<Option<String>, SdkError> {
    Ok(host.kv_get(&repo_key(fleet)).await?.map(|b| String::from_utf8_lossy(&b).into_owned()))
}

pub async fn set_repo_of_fleet(host: &Host, fleet: &str, repo: &str) -> Result<(), SdkError> {
    host.kv_put(&repo_key(fleet), repo.as_bytes(), false).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_plugin_sdk::testing::FakeHost;
    use serde_json::json;

    fn row(repo: &str, number: u64) -> Session {
        Session { repo: repo.into(), installation: 7, kind: Kind::Issue, number, head: None, status_comment: None, session_id: None, last_activity: 0, closed: false }
    }

    #[tokio::test]
    async fn rows_are_mirrored_and_the_reverse_map_survives_a_reload() {
        let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
        let host = Host::new(fake.env("github", std::path::Path::new("scratch"))).unwrap();
        let mut s = Sessions::default();
        s.set(&host, "gh-acme-api/repo/issue-12", row("acme/api", 12)).await.unwrap();
        s.set(&host, "gh-acme-api/repo/pr-3", Session { kind: Kind::Pr, head: Some("f".into()), ..row("acme/api", 3) }).await.unwrap();
        assert_eq!(fake.kv_json("session/gh-acme-api/repo/issue-12").unwrap()["number"], 12);
        let again = Sessions::load(&host).await.unwrap();
        assert_eq!(again.by_number("acme/api", 12), Some("gh-acme-api/repo/issue-12"));
        assert_eq!(again.live_in("gh-acme-api").len(), 2);
        assert_eq!(again.live_in("gh-acme-api")[1].head.as_deref(), Some("f"));
        assert_eq!(again.agents_in("other"), Vec::<String>::new());
        s.remove(&host, "gh-acme-api/repo/issue-12").await.unwrap();
        assert!(fake.kv_json("session/gh-acme-api/repo/issue-12").is_none());
        assert_eq!(s.by_number("acme/api", 12), None);
        set_repo_of_fleet(&host, "gh-acme-api", "acme/api").await.unwrap();
        assert_eq!(repo_of_fleet(&host, "gh-acme-api").await.unwrap().as_deref(), Some("acme/api"));
        assert_eq!(repo_of_fleet(&host, "gh-none").await.unwrap(), None);
    }

    #[test]
    fn an_older_row_without_the_optional_fields_still_loads() {
        let r: Session = serde_json::from_value(json!({ "repo": "a/b", "installation": 1, "kind": "pr", "number": 2 })).unwrap();
        assert!(!r.closed && r.status_comment.is_none() && r.last_activity == 0);
        assert_eq!(fleet_of("gh-a-b/repo/pr-2"), "gh-a-b");
    }
}
```

- [ ] **Step 2: Run and commit**

Add `pub mod session;` to `lib.rs`. Run: `mise run plugin github`
Expected: PASS.

```bash
git add plugins/github
git commit -m "feat(github): session rows mirrored to KV with the reverse map (Spec M §5)"
```

---

### Task 7: The actor, part 1: state, starting a session, the first prompt

**Files:**
- Create: `plugins/github/src/actor.rs`.

**Interfaces:**
- Consumes: everything above; common's `Queue`, `Health`, `Shared`, `Questions`, `Deliveries`, `render`, `PhaseChange`.
- Produces:

```rust
pub use balerix_plugin_common::queue::{Health, QUEUE};
pub type Queue = balerix_plugin_common::queue::Queue<Command>;
pub const TICK: Duration = Duration::from_secs(5);
pub const STATUS_COALESCE: Duration = Duration::from_secs(2);
pub const PERMISSION_TTL: Duration = Duration::from_secs(300);
pub enum Command { Configure(DaemonConfig), Activate { agent, config: AgentConfig }, Deactivate { agent }, Events(Vec<HookEvent>), Phases(Vec<PhaseChange>), Webhook(WebhookEvent), Tick }
pub struct Counters { messages_sent, events_dropped, inbound, errors, answers_mismatched, deliveries, webhooks: IntCounterVec, sessions_open: IntGauge, applies: IntCounterVec, status_edits: IntCounterVec }
pub struct Actor<G: GitHubPort> { … }  // new(host, port, counters, health), load(), run(queue), handle(command)
```

- [ ] **Step 1: The skeleton, the start flow, and its tests**

Write `src/actor.rs` with this structure (the bodies of the outbound, inbound, review and ending paths are Task 8; leave them as `todo!()`-free stubs that do nothing, with a `// Task 8` comment, so this task compiles and its tests pass):

```rust
//! The actor (Spec M §8): the one task that owns every piece of state
//! and executes the command loop. Generic over `GitHubPort` (M-11).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use balerix_api::{HookEvent, PluginAction, Timestamp};
use balerix_plugin_common::delivery::{self, Deliveries, Kind as BodyKind};
use balerix_plugin_common::metrics::Shared;
use balerix_plugin_common::pending::Questions;
use balerix_plugin_common::phases::PhaseChange;
use balerix_plugin_common::render;
use balerix_plugin_sdk::metrics::{IntCounter, IntCounterVec, IntGauge};
use balerix_plugin_sdk::{Host, Metrics, SdkError};
use serde_json::Value;
use tokio::time::Instant;

use crate::config::{AgentConfig, COMMENT_LIMIT, DaemonConfig, Kind};
use crate::github::{CONFUSED, EYES, GitHubError, GitHubPort, HOORAY, IssueInfo, MINUS_ONE, PLUS_ONE, Permission, Target};
use crate::mention;
use crate::prompt;
use crate::repo_config;
use crate::session::{self, Session, Sessions};
use crate::status::Status;
use crate::webhook::{Author, WebhookEvent};

pub use balerix_plugin_common::queue::{Health, QUEUE};
pub type Queue = balerix_plugin_common::queue::Queue<Command>;

/// `main` pushes `Tick` this often: delivery expiry, status flushes, the
/// idle check, row write-back (§8.7, §8.3, §10).
pub const TICK: Duration = Duration::from_secs(5);
/// A status edit waits this long for more lines (§8.3).
pub const STATUS_COALESCE: Duration = Duration::from_secs(2);
/// The collaborator permission cache (§8.1).
pub const PERMISSION_TTL: Duration = Duration::from_secs(300);
/// `retry_once` waits at most this long on `Retry-After`.
const MAX_INLINE_RETRY: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Configure(DaemonConfig),
    Activate { agent: String, config: AgentConfig },
    Deactivate { agent: String },
    Events(Vec<HookEvent>),
    Phases(Vec<PhaseChange>),
    Webhook(WebhookEvent),
    Tick,
}

#[derive(Debug, Clone)]
pub struct Counters {
    pub messages_sent: IntCounterVec,
    pub events_dropped: IntCounter,
    pub inbound: IntCounterVec,
    pub errors: IntCounterVec,
    pub answers_mismatched: IntCounter,
    pub deliveries: IntCounterVec,
    pub webhooks: IntCounterVec,
    pub sessions_open: IntGauge,
    pub applies: IntCounterVec,
    pub status_edits: IntCounterVec,
}

impl Counters {
    pub fn new(metrics: &Metrics) -> Result<Self, SdkError> {
        let shared = Shared::new(metrics)?;
        Ok(Self {
            messages_sent: shared.messages_sent,
            events_dropped: shared.events_dropped,
            inbound: shared.inbound,
            errors: shared.errors,
            answers_mismatched: shared.answers_mismatched,
            deliveries: shared.deliveries,
            webhooks: metrics.int_counter_vec("webhooks_total", "Webhook deliveries, by event and outcome", &["event", "outcome"])?,
            sessions_open: metrics.int_gauge("sessions_open", "Issues and pull requests with a live agent")?,
            applies: metrics.int_counter_vec("applies_total", "Fleet applies, by outcome", &["outcome"])?,
            status_edits: metrics.int_counter_vec("status_edits_total", "Status comment edits, by outcome", &["outcome"])?,
        })
    }
}

/// Where a routed prompt's 👍 or note goes (§8.7).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sent {
    repo: String,
    installation: u64,
    number: u64,
    target: Target,
    agent: String,
    asker: String,
}

/// A review held while a question is open (§9).
#[derive(Debug, Clone)]
struct Held {
    reviewer: String,
    message: String,
}

pub struct Actor<G: GitHubPort> {
    host: Host,
    port: G,
    counters: Counters,
    health: Health,
    config: Option<DaemonConfig>,
    slug: String,
    pending: Vec<Command>,
    agents: HashMap<String, AgentConfig>,
    sessions: Sessions,
    questions: Questions,
    deliveries: Deliveries<Sent>,
    statuses: HashMap<String, Status>,
    status_dirty: HashMap<String, Instant>,
    rows_dirty: HashMap<String, ()>,
    permissions: HashMap<(String, String), (Permission, Instant)>,
    held: HashMap<String, Vec<Held>>,
    ended_notice: HashMap<String, ()>,
    /// What the first prompt needs, kept from `start` until `SessionStart`.
    first_prompts: HashMap<String, FirstPrompt>,
    /// Markers evicted inside a sync path, reported on the next tick.
    unconfirmed_queue: Vec<Sent>,
    /// Wall-clock seconds; a test pins it.
    now_secs: fn() -> u64,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl<G: GitHubPort> Actor<G> {
    pub fn new(host: Host, port: G, counters: Counters, health: Health, slug: String) -> Self {
        Self {
            host, port, counters, health, slug,
            config: None, pending: Vec::new(), agents: HashMap::new(),
            sessions: Sessions::default(), questions: Questions::default(),
            deliveries: Deliveries::default(), statuses: HashMap::new(),
            status_dirty: HashMap::new(), rows_dirty: HashMap::new(),
            permissions: HashMap::new(), held: HashMap::new(), ended_notice: HashMap::new(),
            first_prompts: HashMap::new(), unconfirmed_queue: Vec::new(),
            now_secs: unix_now,
        }
    }

    pub async fn load(&mut self) {
        match Sessions::load(&self.host).await {
            Ok(s) => self.sessions = s,
            Err(e) => tracing::warn!("github: loading sessions: {e}"),
        }
        match Questions::load(&self.host).await {
            Ok(q) => self.questions = q,
            Err(e) => tracing::warn!("github: loading questions: {e}"),
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
            Command::Activate { agent, config } => self.on_activate(agent, config).await,
            Command::Deactivate { agent } => {
                self.agents.remove(&agent);
                self.questions.clear(&self.host, &agent).await;
                self.deliveries.forget(&agent);
                self.statuses.remove(&agent);
                self.status_dirty.remove(&agent);
                self.held.remove(&agent);
                if let Err(e) = self.sessions.remove(&self.host, &agent).await {
                    tracing::warn!("github: forgetting {agent}: {e}");
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
            Command::Webhook(ev) => self.on_webhook(ev).await,
            Command::Tick => self.on_tick().await,
        }
    }

    fn cfg(&self) -> &DaemonConfig {
        // `handle` buffers everything until `Configure`; this is only
        // reached after it.
        self.config.as_ref().unwrap_or_else(|| unreachable!("configured"))
    }

    fn publish_gauges(&self) {
        self.counters.sessions_open.set(self.sessions.open_count() as i64);
    }

    /// §5: a row missing from KV is rebuilt from the activation, with
    /// `status_comment` unset; the first status edit then posts anew.
    async fn on_activate(&mut self, agent: String, config: AgentConfig) {
        if self.sessions.get(&agent).is_none()
            && let (Some(kind), Some(number)) = (config.kind, config.number)
            && let Ok(Some(repo)) = session::repo_of_fleet(&self.host, session::fleet_of(&agent)).await
        {
            let row = Session { repo, installation: 0, kind, number, head: None, status_comment: None, session_id: None, last_activity: (self.now_secs)(), closed: false };
            if let Err(e) = self.sessions.set(&self.host, &agent, row).await {
                tracing::warn!("github: rebuilding row for {agent}: {e}");
            }
        }
        self.agents.insert(agent, config);
        self.publish_gauges();
    }

    // ---- §8.1 who is heard ----

    async fn permitted(&mut self, installation: u64, repo: &str, author: &Author) -> bool {
        if author.bot || author.login.eq_ignore_ascii_case(&format!("{}[bot]", self.slug)) || author.login.eq_ignore_ascii_case(&self.slug) {
            return false;
        }
        let key = (repo.to_string(), author.login.clone());
        let now = Instant::now();
        if let Some((p, until)) = self.permissions.get(&key) && *until > now {
            return p.may_prompt();
        }
        let p = match retry_once(|| self.port.permission(installation, repo, &author.login)).await {
            Ok(p) => p,
            Err(GitHubError::NotFound) => Permission::None,
            Err(e) => {
                self.counters.errors.with_label_values(&["permission"]).inc();
                tracing::warn!("github: permission of {} on {repo}: {e}", author.login);
                return false;
            }
        };
        self.permissions.insert(key, (p, now + PERMISSION_TTL));
        p.may_prompt()
    }

    // ---- §7 → §8 dispatch ----

    async fn on_webhook(&mut self, ev: WebhookEvent) {
        match ev {
            WebhookEvent::IssueOpened { repo, installation, number, author, title, body, url } => {
                self.on_opening(installation, &repo, Kind::Issue, number, author, &title, &body, &url, None, Target::Issue(number), None).await;
            }
            WebhookEvent::PrOpened { repo, installation, number, author, title, body, url, head, head_repo, base } => {
                self.on_opening(installation, &repo, Kind::Pr, number, author, &title, &body, &url, Some((head, head_repo, base)), Target::Issue(number), None).await;
            }
            WebhookEvent::Comment { repo, installation, number, author, comment_id, body, is_pr } => {
                self.on_comment(installation, &repo, number, author, comment_id, &body, is_pr).await;
            }
            WebhookEvent::Closed { repo, installation, number, merged } => {
                if let Some(agent) = self.sessions.by_number(&repo, number).map(str::to_string) {
                    self.end(&agent, installation, if merged { "merged" } else { "closed" }).await;
                }
            }
            WebhookEvent::ReviewSubmitted { repo, installation, number, author, review_id, state, body, commit } => {
                self.on_review(installation, &repo, number, author, review_id, &state, &body, &commit).await;
            }
        }
    }

    /// An issue or PR body that mentions the App starts a session.
    #[allow(clippy::too_many_arguments)]
    async fn on_opening(&mut self, installation: u64, repo: &str, kind: Kind, number: u64, author: Author, title: &str, body: &str, url: &str, pr: Option<(String, String, String)>, target: Target, comment: Option<&str>) {
        if !mention::mentions(body, &self.slug) && !comment.is_some_and(|c| mention::mentions(c, &self.slug)) {
            return;
        }
        if !self.permitted(installation, repo, &author).await {
            self.counters.inbound.with_label_values(&["unpermitted"]).inc();
            return;
        }
        self.start(installation, repo, kind, number, &author.login, title, body, url, pr, target, comment).await;
    }

    async fn on_comment(&mut self, installation: u64, repo: &str, number: u64, author: Author, comment_id: u64, body: &str, is_pr: bool) {
        let count = |c: &Counters, outcome: &str| c.inbound.with_label_values(&[outcome]).inc();
        if author.bot || author.login.eq_ignore_ascii_case(&format!("{}[bot]", self.slug)) {
            count(&self.counters, "own_or_bot");
            return;
        }
        if !self.permitted(installation, repo, &author).await {
            count(&self.counters, "unpermitted");
            return;
        }
        let live = self.sessions.by_number(repo, number).map(str::to_string);
        let closed = live.as_deref().and_then(|a| self.sessions.get(a)).is_some_and(|r| r.closed);
        let mentioned = mention::mentions(body, &self.slug);
        match (live, closed) {
            (Some(agent), false) => self.on_message(&agent, installation, repo, number, &author.login, comment_id, body).await, // Task 8
            (Some(agent), true) if !mentioned => {
                count(&self.counters, "ended");
                self.react(installation, repo, Target::Comment(comment_id), CONFUSED).await;
                if self.ended_notice.insert(agent.clone(), ()).is_none() {
                    self.post(installation, repo, number, &format!("this session has ended; mention @{} to start a new one", self.slug), "notice").await;
                }
            }
            _ if mentioned => {
                // No live session: start one. A comment carries no title
                // or body, so the issue is read (§8.2's prompt needs it,
                // and a PR's head decides the fork check and the branch).
                let info = match retry_once(|| self.port.issue(installation, repo, number)).await {
                    Ok(i) => i,
                    Err(e) => { self.github_failed("issue", &e); return; }
                };
                let kind = if is_pr || info.pr.is_some() { Kind::Pr } else { Kind::Issue };
                let IssueInfo { title, body: issue_body, url, pr } = info;
                self.start(installation, repo, kind, number, &author.login, &title, &issue_body, &url, pr, Target::Comment(comment_id), Some(body)).await;
            }
            _ => count(&self.counters, "unknown_number"),
        }
    }

    // ---- §8.2 starting ----

    #[allow(clippy::too_many_arguments)]
    async fn start(&mut self, installation: u64, repo: &str, kind: Kind, number: u64, asker: &str, title: &str, body: &str, url: &str, pr: Option<(String, String, String)>, target: Target, comment: Option<&str>) {
        self.react(installation, repo, target, EYES).await;
        if let Some((_, head_repo, _)) = &pr && !head_repo.eq_ignore_ascii_case(repo) {
            self.post(installation, repo, number, "sessions on pull requests from forks are not supported", "notice").await;
            self.react(installation, repo, target, CONFUSED).await;
            return;
        }
        let fleet = mention::fleet_name(repo);
        match session::repo_of_fleet(&self.host, &fleet).await {
            Ok(Some(other)) if !other.eq_ignore_ascii_case(repo) => {
                self.post(installation, repo, number, &format!("fleet name {fleet} already stands for {other}"), "notice").await;
                self.react(installation, repo, target, CONFUSED).await;
                return;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("github: reading repo/{fleet}: {e}");
                return;
            }
        }
        let agent = format!("{fleet}/repo/{}", kind.agent_name(number));
        let resumed = self.sessions.get(&agent).is_some();
        let head = pr.as_ref().map(|(h, _, _)| h.clone());
        let base = pr.as_ref().map(|(_, _, b)| b.clone());

        // §6: the file, then the apply with this session added
        let mut live = self.sessions.live_in(&fleet);
        live.retain(|l| l.number != number);
        live.push(repo_config::Live { kind, number, head: head.clone() });
        let default_branch = match retry_once(|| self.port.default_branch(installation, repo)).await {
            Ok(b) => b,
            Err(e) => { self.github_failed("default_branch", &e); return; }
        };
        let text = match retry_once(|| self.port.read_file(installation, repo, &self.cfg().config_path, &default_branch)).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                self.refuse(installation, repo, number, target, &format!("{} not found on {default_branch}", self.cfg().config_path), "config").await;
                return;
            }
            Err(e) => { self.github_failed("read_file", &e); return; }
        };
        let file = match repo_config::prepare(&text, repo, &fleet, &default_branch, &live) {
            Ok(f) => f,
            Err(m) => { self.refuse(installation, repo, number, target, &m, "config").await; return; }
        };
        if let Err(e) = self.host.apply_fleet(&fleet, &file).await {
            self.refuse(installation, repo, number, target, &e.to_string(), "daemon").await;
            return;
        }
        self.counters.applies.with_label_values(&["ok"]).inc();
        self.health.ok();
        if let Err(e) = session::set_repo_of_fleet(&self.host, &fleet, repo).await {
            tracing::warn!("github: writing repo/{fleet}: {e}");
        }

        // §8.3: the status comment, then the row
        let mut status = self.statuses.remove(&agent).unwrap_or_else(|| Status::new(&agent));
        status.phase = "starting".into();
        status.push((self.now_secs)(), if resumed { "resuming" } else { "starting" });
        let status_comment = match retry_once(|| self.port.comment(installation, repo, number, &status.render())).await {
            Ok(id) => Some(id),
            Err(e) => { self.github_failed("comment", &e); None }
        };
        self.statuses.insert(agent.clone(), status);
        let row = Session { repo: repo.to_string(), installation, kind, number, head, status_comment, session_id: None, last_activity: (self.now_secs)(), closed: false };
        if let Err(e) = self.sessions.set(&self.host, &agent, row).await {
            tracing::warn!("github: storing row for {agent}: {e}");
        }
        self.ended_notice.remove(&agent);
        // The first prompt waits for `SessionStart` (§8.2); what it needs is kept here.
        self.first_prompts.insert(agent.clone(), FirstPrompt {
            title: title.to_string(), body: body.to_string(), url: url.to_string(),
            branch: head_or_default(&fleet, kind, number, pr.as_ref()), base: base.unwrap_or_else(|| default_branch.clone()),
            asker: asker.to_string(), comment: comment.map(str::to_string), resumed, target,
        });
        self.publish_gauges();
    }

    async fn refuse(&mut self, installation: u64, repo: &str, number: u64, target: Target, message: &str, outcome: &str) {
        self.counters.applies.with_label_values(&[outcome]).inc();
        self.post(installation, repo, number, message, "notice").await;
        self.react(installation, repo, target, CONFUSED).await;
    }

    fn github_failed(&self, kind: &str, e: &GitHubError) {
        self.counters.errors.with_label_values(&[kind]).inc();
        tracing::warn!("github: {kind}: {e}");
        if matches!(e, GitHubError::Auth(_)) {
            self.health.fail(format!("github: {e}"));
        }
    }

    // ---- posting helpers ----

    /// Posts `body` split into comments (§8.5); a failed part stops the
    /// rest. Answers the first comment's id.
    async fn post(&self, installation: u64, repo: &str, number: u64, body: &str, kind: &str) -> Option<u64> {
        let max_parts = self.config.as_ref().map_or(crate::config::DEFAULT_MAX_PARTS, |c| c.max_parts);
        let mut first = None;
        for part in render::split(body, COMMENT_LIMIT, max_parts) {
            match retry_once(|| self.port.comment(installation, repo, number, &part)).await {
                Ok(id) => { self.counters.messages_sent.with_label_values(&[kind]).inc(); self.health.ok(); first.get_or_insert(id); }
                Err(e) => { self.github_failed("comment", &e); return first; }
            }
        }
        first
    }

    async fn react(&self, installation: u64, repo: &str, target: Target, content: &str) {
        if let Err(e) = self.port.react(installation, repo, target, content).await {
            self.counters.errors.with_label_values(&["react"]).inc();
            tracing::warn!("github: reacting {content} on {target:?}: {e}");
        }
    }
}

/// What the first prompt needs, kept from the start until `SessionStart`.
#[derive(Debug, Clone)]
struct FirstPrompt {
    title: String, body: String, url: String, branch: String, base: String,
    asker: String, comment: Option<String>, resumed: bool, target: Target,
}

fn head_or_default(fleet: &str, kind: Kind, number: u64, pr: Option<&(String, String, String)>) -> String {
    pr.map(|(h, _, _)| h.clone()).unwrap_or_else(|| format!("balerix/{fleet}/repo/{}", kind.agent_name(number)))
}

/// One retry after `Retry-After`, when it is short (matrix's rule).
async fn retry_once<T, F, Fut>(call: F) -> Result<T, GitHubError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, GitHubError>>,
{
    let attempt = call().await;
    if let Err(GitHubError::RateLimited { retry_after_ms }) = attempt {
        let delay = Duration::from_millis(retry_after_ms);
        if delay > MAX_INLINE_RETRY {
            tracing::warn!("github: rate limited for {retry_after_ms} ms; not waiting");
            return attempt;
        }
        tokio::time::sleep(delay).await;
        return call().await;
    }
    attempt
}
```

`on_message`, `on_review`, `end`, `on_event`, `on_phase`, `on_tick` are Task 8: for now, `on_event`/`on_phase`/`on_tick` are empty `async fn`s, `on_message`/`on_review`/`end` take their arguments and do nothing (`let _ = (…)`), each with `// Task 8`.

Tests for this task, in `actor.rs`'s `tests` module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::fake::{Call, FakePort};
    use balerix_plugin_sdk::testing::{FakeHost, event};
    use serde_json::json;

    pub(super) const FILE: &str = "apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  repo:\n    repo: acme/api\n    agents: {}\n";

    fn counters() -> Counters { Counters::new(&Metrics::new("github")).unwrap() }

    pub(super) fn daemon_config() -> DaemonConfig {
        crate::config::parse_daemon(&json!({ "appId": 1, "privateKey": "k", "webhookSecret": "s" })).unwrap()
    }

    pub(super) async fn actor() -> (FakeHost, FakePort, Actor<FakePort>) {
        let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
        let host = Host::new(fake.env("github", std::path::Path::new("scratch"))).unwrap();
        let port = FakePort::new("balerix");
        port.set_permission("acme/api", "alice", Permission::Write);
        port.set_permission("acme/api", "bob", Permission::Maintain);
        port.set_permission("acme/api", "eve", Permission::Read);
        port.set_file("acme/api", "main", ".balerix.yaml", FILE);
        let mut a = Actor::new(host, port.clone(), counters(), Health::new(), "balerix".into());
        a.now_secs = || 14 * 3600 + 2 * 60;
        a.handle(Command::Configure(daemon_config())).await;
        (fake, port, a)
    }

    pub(super) fn user(login: &str) -> Author { Author { login: login.into(), bot: false } }

    pub(super) fn comment(number: u64, login: &str, id: u64, body: &str) -> Command {
        Command::Webhook(WebhookEvent::Comment { repo: "acme/api".into(), installation: 7, number, author: user(login), comment_id: id, body: body.into(), is_pr: false })
    }

    pub(super) fn reactions(calls: &[Call]) -> Vec<(Target, String)> {
        calls.iter().filter_map(|c| match c { Call::React { target, content, .. } => Some((*target, content.clone())), _ => None }).collect()
    }

    pub(super) fn comments(calls: &[Call]) -> Vec<(u64, String)> {
        calls.iter().filter_map(|c| match c { Call::Comment { number, body, .. } => Some((*number, body.clone())), _ => None }).collect()
    }

    #[tokio::test]
    async fn a_mention_by_a_collaborator_starts_a_session_and_the_applied_file_carries_the_agent() {
        let (fake, port, mut a) = actor().await;
        a.handle(comment(12, "alice", 5, "@balerix please look at this")).await;
        let applied = fake.applied_fleets();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].0, "gh-acme-api");
        let file = &applied[0].1;
        assert_eq!(file["name"], "gh-acme-api");
        assert_eq!(file["crews"]["repo"]["ref"], "main");
        assert_eq!(file["crews"]["repo"]["agents"]["issue-12"]["plugins"]["github"], json!({ "kind": "issue", "number": 12 }));
        assert_eq!(reactions(&port.calls()), vec![(Target::Comment(5), EYES.to_string())]);
        let c = comments(&port.calls());
        assert_eq!(c.len(), 1, "the status comment: {c:?}");
        assert!(c[0].1.starts_with("**balerix** · `gh-acme-api/repo/issue-12` · **starting**"), "{}", c[0].1);
        assert!(c[0].1.contains("- 14:02 starting"));
        let row = fake.kv_json("session/gh-acme-api/repo/issue-12").unwrap();
        assert_eq!(row["status_comment"], 1001);
        assert_eq!(fake.kv_json("repo/gh-acme-api"), None, "the repo key is bytes, not JSON");
        assert_eq!(fake.kv().get("repo/gh-acme-api").map(|(b, _)| b.clone()), Some(b"acme/api".to_vec()));
        assert!(fake.actions_for("gh-acme-api/repo/issue-12").is_empty(), "no prompt before SessionStart");
        assert_eq!(a.sessions.open_count(), 1);
    }

    #[tokio::test]
    async fn a_non_collaborator_a_bot_and_the_apps_own_comments_are_silent() {
        let (fake, port, mut a) = actor().await;
        a.handle(comment(12, "eve", 5, "@balerix go")).await;
        a.handle(comment(12, "nobody", 6, "@balerix go")).await;
        a.handle(Command::Webhook(WebhookEvent::Comment { repo: "acme/api".into(), installation: 7, number: 12, author: Author { login: "dependabot[bot]".into(), bot: true }, comment_id: 7, body: "@balerix go".into(), is_pr: false })).await;
        a.handle(Command::Webhook(WebhookEvent::Comment { repo: "acme/api".into(), installation: 7, number: 12, author: Author { login: "balerix[bot]".into(), bot: true }, comment_id: 8, body: "**balerix** status".into(), is_pr: false })).await;
        assert!(fake.applied_fleets().is_empty());
        assert!(reactions(&port.calls()).is_empty() && comments(&port.calls()).is_empty());
        assert_eq!(a.counters.inbound.with_label_values(&["unpermitted"]).get(), 2);
        assert_eq!(a.counters.inbound.with_label_values(&["own_or_bot"]).get(), 2);
    }

    #[tokio::test]
    async fn a_fork_pr_is_refused_and_a_pr_gets_its_head_as_branch() {
        let (fake, port, mut a) = actor().await;
        let pr = |head_repo: &str| Command::Webhook(WebhookEvent::PrOpened { repo: "acme/api".into(), installation: 7, number: 34, author: user("alice"), title: "t".into(), body: "@balerix review".into(), url: "u".into(), head: "feature/x".into(), head_repo: head_repo.into(), base: "main".into() });
        a.handle(pr("someone/api")).await;
        assert!(fake.applied_fleets().is_empty());
        assert_eq!(comments(&port.calls()), vec![(34, "sessions on pull requests from forks are not supported".into())]);
        assert_eq!(reactions(&port.calls()), vec![(Target::Issue(34), EYES.into()), (Target::Issue(34), CONFUSED.into())]);
        port.take_calls();
        a.handle(pr("acme/api")).await;
        let file = &fake.applied_fleets()[0].1;
        assert_eq!(file["crews"]["repo"]["agents"]["pr-34"]["branch"], "feature/x");
    }

    #[tokio::test]
    async fn a_config_error_is_posted_once_and_applies_nothing() {
        let (fake, port, mut a) = actor().await;
        port.set_file("acme/api", "main", ".balerix.yaml", "apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  repo: { repo: other/thing }\n");
        a.handle(comment(12, "alice", 5, "@balerix go")).await;
        assert!(fake.applied_fleets().is_empty());
        let c = comments(&port.calls());
        assert_eq!(c.len(), 1);
        assert!(c[0].1.starts_with(".balerix.yaml: crews.repo.repo: expected acme/api"), "{}", c[0].1);
        assert_eq!(reactions(&port.calls()).last().map(|r| r.1.as_str()), Some(CONFUSED));
        assert!(fake.kv_json("session/gh-acme-api/repo/issue-12").is_none());
        assert_eq!(a.counters.applies.with_label_values(&["config"]).get(), 1);
    }

    #[tokio::test]
    async fn a_daemon_refusal_is_posted_verbatim() {
        let (fake, port, mut a) = actor().await;
        fake.fail_manage(Some((400, "defaults.sandbox: not allowed in a plugin-applied fleet file; the host's default applies")));
        a.handle(comment(12, "alice", 5, "@balerix go")).await;
        let c = comments(&port.calls());
        assert_eq!(c[0].1, "daemon: HTTP 400: defaults.sandbox: not allowed in a plugin-applied fleet file; the host's default applies");
        assert_eq!(a.counters.applies.with_label_values(&["daemon"]).get(), 1);
    }

    #[tokio::test]
    async fn a_name_collision_is_refused() {
        let (fake, port, mut a) = actor().await;
        let host = Host::new(fake.env("github", std::path::Path::new("scratch"))).unwrap();
        // a repository whose name sanitises the same already owns the fleet name
        session::set_repo_of_fleet(&host, "gh-acme-api", "acme/api.").await.unwrap();
        a.handle(comment(12, "alice", 5, "@balerix go")).await;
        assert!(fake.applied_fleets().is_empty());
        assert_eq!(comments(&port.calls())[0].1, "fleet name gh-acme-api already stands for acme/api.");
    }

    #[tokio::test]
    async fn the_permission_check_is_cached_for_five_minutes() {
        let (_fake, port, mut a) = actor().await;
        a.handle(comment(12, "alice", 5, "@balerix go")).await;
        a.handle(comment(12, "alice", 6, "and this")).await;
        let checks = port.calls().iter().filter(|c| matches!(c, Call::Permission { .. })).count();
        assert_eq!(checks, 1);
    }

    #[tokio::test]
    async fn a_mention_in_the_issue_body_starts_and_reacts_on_the_issue() {
        let (fake, port, mut a) = actor().await;
        a.handle(Command::Webhook(WebhookEvent::IssueOpened { repo: "acme/api".into(), installation: 7, number: 12, author: user("bob"), title: "Bug".into(), body: "@balerix fix".into(), url: "u".into() })).await;
        assert_eq!(fake.applied_fleets().len(), 1);
        assert_eq!(reactions(&port.calls())[0], (Target::Issue(12), EYES.into()));
    }
}
```

The `on_comment` start path reads the issue through the port, so add to `GitHubPort` now:

```rust
    /// Title, body, URL and, for a PR, `(head, head_repo, base)`.
    fn issue(&self, installation: u64, repo: &str, number: u64) -> impl Future<Output = Result<IssueInfo, GitHubError>> + Send;
```

with `#[derive(Debug, Clone, Default, PartialEq, Eq)] pub struct IssueInfo { pub title: String, pub body: String, pub url: String, pub pr: Option<(String, String, String)> }` in `github.rs`, `Call::Issue { repo, number }`, and `FakePort::set_issue(repo, number, IssueInfo)` (the default answer: empty title and body, `https://github.com/<repo>/issues/<n>`, `pr: None`). The fork check and the branch then work the same for a PR started from a comment as for one started from its body.

- [ ] **Step 2: Run and commit**

Add `pub mod actor;` to `lib.rs`. Run: `mise run plugin github`
Expected: PASS.

```bash
git add plugins/github
git commit -m "feat(github): the actor's state and the start of a session (Spec M §8.1, §8.2, §6)"
```

---

### Task 8: The actor, part 2: outbound, inbound, reviews, ending, the tick

**Files:**
- Modify: `plugins/github/src/actor.rs` (fill the Task 7 stubs; add the tests below).

**Interfaces:**
- Consumes: `answer::{on_reply, on_closed, Decision, Reaction, Verdict}`, `question`, `pending::{Questions, Stage}`, `render::{event_message, question_message, phase_message}`, `delivery`, `prompt`, `status`.
- Produces: the complete `Actor` behaviour of Spec M §8.3 to §10.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module (helpers from Task 7 are `pub(super)`):

```rust
    async fn started() -> (FakeHost, FakePort, Actor<FakePort>) {
        let (fake, port, mut a) = actor().await;
        a.handle(comment(12, "alice", 5, "@balerix please look")).await;
        a.handle(Command::Activate { agent: AGENT.into(), config: crate::config::parse_agent(&json!({ "kind": "issue", "number": 12 })).unwrap() }).await;
        let mut e = event(AGENT, "SessionStart", json!({ "source": "startup" }));
        e.session_id = Some("0199aa11-abcd".into());
        a.handle(Command::Events(vec![e])).await;
        port.take_calls();
        (fake, port, a)
    }
    const AGENT: &str = "gh-acme-api/repo/issue-12";

    fn during(name: &str, payload: Value) -> HookEvent {
        let mut e = event(AGENT, name, payload);
        e.session_id = Some("0199aa11-abcd".into());
        e
    }

    fn edits(calls: &[Call]) -> Vec<String> {
        calls.iter().filter_map(|c| match c { Call::EditComment { body, .. } => Some(body.clone()), _ => None }).collect()
    }

    #[tokio::test]
    async fn the_first_prompt_goes_out_on_session_start_and_never_before() {
        let (fake, port, mut a) = actor().await;
        a.handle(comment(12, "alice", 5, "@balerix please look")).await;
        a.handle(Command::Activate { agent: AGENT.into(), config: crate::config::parse_agent(&json!({ "kind": "issue", "number": 12 })).unwrap() }).await;
        a.handle(Command::Events(vec![during("Notification", json!({ "message": "early" }))])).await;
        assert!(fake.actions_for(AGENT).is_empty());
        a.handle(Command::Events(vec![during("SessionStart", json!({ "source": "startup" }))])).await;
        let actions = fake.actions_for(AGENT);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            PluginAction::SendText { text, submit } => {
                assert!(*submit);
                insta::assert_snapshot!(text);
                assert!(text.contains("issue #12") && text.contains("@alice asked:") && text.contains("please look"));
            }
            other => panic!("{other:?}"),
        }
        // the status comment was edited, not re-posted: header names the session and the line
        let e = edits(&port.calls());
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(e[0].contains("session `0199aa11`") && e[0].contains("session started (startup)"), "{}", e[0]);
        // the mention's 👍 arrives with the matching submit (§8.7)
        a.handle(Command::Events(vec![during("UserPromptSubmit", json!({ "prompt": actions_text(&actions[0]) }))])).await;
        assert_eq!(reactions(&port.calls()).last(), Some(&(Target::Comment(5), PLUS_ONE.to_string())));
    }

    fn actions_text(a: &PluginAction) -> String {
        match a { PluginAction::SendText { text, .. } => text.clone(), _ => String::new() }
    }

    #[tokio::test]
    async fn a_permitted_comment_while_live_is_sent_eyes_then_plus_one() {
        let (fake, port, mut a) = started().await;
        a.handle(comment(12, "bob", 9, "also check the tests")).await;
        assert_eq!(fake.actions_for(AGENT).last(), Some(&PluginAction::SendText { text: "also check the tests".into(), submit: true }));
        assert_eq!(reactions(&port.calls()), vec![(Target::Comment(9), EYES.into())]);
        a.handle(Command::Events(vec![during("UserPromptSubmit", json!({ "prompt": "also  check the tests" }))])).await;
        assert_eq!(reactions(&port.calls()), vec![(Target::Comment(9), EYES.into()), (Target::Comment(9), PLUS_ONE.into())]);
    }

    #[tokio::test]
    async fn a_failed_send_posts_the_error_and_minus_one() {
        let (fake, port, mut a) = started().await;
        fake.fail_actions(Some("window gone"));
        a.handle(comment(12, "bob", 9, "hello")).await;
        assert!(comments(&port.calls()).iter().any(|(_, b)| b.starts_with("not delivered to gh-acme-api/repo/issue-12: ")));
        assert_eq!(reactions(&port.calls()), vec![(Target::Comment(9), MINUS_ONE.into())]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unconfirmed_prompt_gets_confused_and_a_status_line() {
        let (_fake, port, mut a) = started().await;
        a.handle(comment(12, "bob", 9, "hello")).await;
        tokio::time::advance(Duration::from_secs(31)).await;
        a.handle(Command::Tick).await;
        assert_eq!(reactions(&port.calls()).last(), Some(&(Target::Comment(9), CONFUSED.into())));
        tokio::time::advance(STATUS_COALESCE).await;
        a.handle(Command::Tick).await;
        assert!(edits(&port.calls()).last().unwrap().contains("prompt from @bob not confirmed after 30s"));
    }

    #[tokio::test]
    async fn a_slash_command_gets_eyes_only() {
        let (_fake, port, mut a) = started().await;
        a.handle(comment(12, "bob", 9, "/compact")).await;
        assert_eq!(reactions(&port.calls()), vec![(Target::Comment(9), EYES.into())]);
        assert_eq!(a.deliveries.pending(AGENT), 0);
    }

    #[tokio::test]
    async fn a_stop_posts_the_message_and_a_long_one_is_split_and_a_failed_part_stops_the_rest() {
        let (_fake, port, mut a) = started().await;
        a.handle(Command::Events(vec![during("Stop", json!({ "last_assistant_message": "Done: I fixed the refund path." }))])).await;
        assert_eq!(comments(&port.calls()), vec![(12, "Done: I fixed the refund path.".into())]);
        port.take_calls();
        let long = "abcd\n".repeat(30_000);
        a.handle(Command::Events(vec![during("Stop", json!({ "last_assistant_message": long }))])).await;
        let c = comments(&port.calls());
        assert!(c.len() >= 2 && c.len() <= 10, "{}", c.len());
        assert!(c[0].1.ends_with(&format!("(1/{})", c.len())));
        port.take_calls();
        port.fail_next(GitHubError::Other("boom".into()));
        a.handle(Command::Events(vec![during("Stop", json!({ "last_assistant_message": "abcd\n".repeat(30_000) }))])).await;
        assert!(comments(&port.calls()).is_empty(), "the first part failed; nothing after it");
        // an empty message posts nothing and adds a status line
        port.take_calls();
        a.handle(Command::Events(vec![during("Stop", json!({}))])).await;
        assert!(comments(&port.calls()).is_empty());
        assert!(a.statuses[AGENT].render().contains("turn finished"));
    }

    #[tokio::test(start_paused = true)]
    async fn the_status_comment_is_edited_not_reposted_and_coalesced_and_reposted_when_deleted() {
        let (_fake, port, mut a) = started().await;
        a.handle(Command::Events(vec![during("Notification", json!({ "message": "Claude needs your permission to use Bash", "notification_type": "permission_prompt" }))])).await;
        a.handle(Command::Events(vec![during("Notification", json!({ "message": "second" }))])).await;
        assert!(edits(&port.calls()).is_empty(), "coalesced: nothing yet");
        tokio::time::advance(STATUS_COALESCE).await;
        a.handle(Command::Tick).await;
        let e = edits(&port.calls());
        assert_eq!(e.len(), 1, "one edit for two lines");
        assert!(e[0].contains("needs you: Claude needs your permission to use Bash") && e[0].contains("- 14:02 second"));
        assert!(comments(&port.calls()).is_empty());
        port.take_calls();
        port.delete_comment(1001);
        a.handle(Command::Events(vec![during("Notification", json!({ "message": "third" }))])).await;
        tokio::time::advance(STATUS_COALESCE).await;
        a.handle(Command::Tick).await;
        assert_eq!(comments(&port.calls()).len(), 1, "a new status comment");
        assert_eq!(a.sessions.get(AGENT).unwrap().status_comment, Some(1002));
    }

    #[tokio::test]
    async fn a_question_round_trip_through_common() {
        use balerix_plugin_common::question::fixtures::{color, input};
        let (fake, port, mut a) = started().await;
        a.handle(Command::Events(vec![during("PreToolUse", json!({ "tool_name": "AskUserQuestion", "tool_input": input(&[color()]) }))])).await;
        let c = comments(&port.calls());
        assert_eq!(c.len(), 1);
        assert!(c[0].1.contains("**question**") && c[0].1.contains("Reply with a comment: a number or a label"), "{}", c[0].1);
        port.take_calls();
        a.handle(comment(12, "alice", 20, "3")).await;
        assert!(matches!(fake.actions_for(AGENT).last(), Some(PluginAction::SendKeys { .. })));
        let c = comments(&port.calls());
        assert_eq!(c[0].1, "**answering** Color → Blue");
        assert_eq!(reactions(&port.calls()), vec![(Target::Comment(20), PLUS_ONE.into())]);
        let echo = c[0].0; // the echo's number; its id is the last minted
        let _ = echo;
        port.take_calls();
        a.handle(Command::Events(vec![during("PostToolUse", json!({ "tool_name": "AskUserQuestion", "tool_response": { "answers": { "Which color?": "Blue" } } }))])).await;
        assert_eq!(reactions(&port.calls()), vec![(Target::Comment(1003), HOORAY.into())], "✓ on the echo");
    }

    #[tokio::test]
    async fn a_review_renders_one_message_and_waits_behind_an_open_question() {
        use balerix_plugin_common::question::fixtures::{color, input};
        use crate::github::ReviewComment;
        let (fake, port, mut a) = actor().await;
        let pr = Command::Webhook(WebhookEvent::PrOpened { repo: "acme/api".into(), installation: 7, number: 34, author: user("alice"), title: "t".into(), body: "@balerix review".into(), url: "u".into(), head: "feature/x".into(), head_repo: "acme/api".into(), base: "main".into() });
        a.handle(pr).await;
        let agent = "gh-acme-api/repo/pr-34";
        a.handle(Command::Activate { agent: agent.into(), config: crate::config::parse_agent(&json!({ "kind": "pr", "number": 34 })).unwrap() }).await;
        let mut e = event(agent, "SessionStart", json!({ "source": "startup" }));
        e.session_id = Some("s".into());
        a.handle(Command::Events(vec![e])).await;
        port.set_review_comments("acme/api", 9, vec![ReviewComment { path: "src/lib.rs".into(), side: "RIGHT".into(), line: Some(42), original_line: None, diff_hunk: "+    let x = foo();".into(), body: "panics".into() }]);
        let review = || Command::Webhook(WebhookEvent::ReviewSubmitted { repo: "acme/api".into(), installation: 7, number: 34, author: user("bob"), review_id: 9, state: "changes_requested".into(), body: "Close.".into(), commit: "3f9c2a1dead".into() });
        a.handle(review()).await;
        let sent = fake.actions_for(agent);
        let text = actions_text(sent.last().unwrap());
        assert!(text.starts_with("Review by @bob: changes requested, at 3f9c2a1\n"), "{text}");
        assert!(text.contains("src/lib.rs line 42 (new)") && text.contains("Overall:\nClose."));
        assert!(a.statuses[agent].render().contains("review from @bob delivered"));
        // held behind a question
        let mut q = event(agent, "PreToolUse", json!({ "tool_name": "AskUserQuestion", "tool_input": input(&[color()]) }));
        q.session_id = Some("s".into());
        a.handle(Command::Events(vec![q])).await;
        let before = fake.actions_for(agent).len();
        a.handle(review()).await;
        assert_eq!(fake.actions_for(agent).len(), before, "held");
        assert!(a.statuses[agent].render().contains("review from @bob held"));
        let mut done = event(agent, "PostToolUse", json!({ "tool_name": "AskUserQuestion", "tool_response": { "answers": {} } }));
        done.session_id = Some("s".into());
        a.handle(Command::Events(vec![done])).await;
        assert_eq!(fake.actions_for(agent).len(), before + 1, "delivered when the question cleared");
        // a stranger's review is ignored
        a.handle(Command::Webhook(WebhookEvent::ReviewSubmitted { repo: "acme/api".into(), installation: 7, number: 34, author: user("eve"), review_id: 10, state: "approved".into(), body: "lgtm".into(), commit: "c".into() })).await;
        assert_eq!(fake.actions_for(agent).len(), before + 1);
    }

    #[tokio::test]
    async fn close_removes_the_agent_and_a_later_comment_is_told_the_session_ended() {
        let (fake, port, mut a) = started().await;
        a.handle(Command::Webhook(WebhookEvent::Closed { repo: "acme/api".into(), installation: 7, number: 12, merged: false })).await;
        let applied = fake.applied_fleets();
        assert_eq!(applied.len(), 2);
        assert_eq!(applied[1].1["crews"]["repo"]["agents"], json!({}), "applied without the agent");
        assert!(a.sessions.get(AGENT).unwrap().closed);
        assert!(a.statuses[AGENT].render().contains("closed"));
        port.take_calls();
        a.handle(comment(12, "bob", 30, "one more thing")).await;
        a.handle(comment(12, "bob", 31, "and another")).await;
        assert_eq!(reactions(&port.calls()), vec![(Target::Comment(30), CONFUSED.into()), (Target::Comment(31), CONFUSED.into())]);
        assert_eq!(comments(&port.calls()), vec![(12, "this session has ended; mention @balerix to start a new one".into())], "once");
        assert!(fake.actions_for(AGENT).len() == 1, "nothing sent after the close");
    }

    #[tokio::test]
    async fn the_idle_tick_ends_the_session_and_the_next_mention_resumes_with_the_branch_line() {
        let (fake, port, mut a) = started().await;
        a.now_secs = || 14 * 3600 + 2 * 60 + 2 * 3600 + 1;
        a.handle(Command::Tick).await;
        assert!(a.sessions.get(AGENT).unwrap().closed);
        assert!(a.statuses[AGENT].render().contains("stopped after 2h idle — mention @balerix to resume"));
        assert_eq!(fake.applied_fleets().len(), 2);
        port.take_calls();
        a.handle(comment(12, "alice", 40, "@balerix continue")).await;
        assert_eq!(fake.applied_fleets().len(), 3);
        assert!(!a.sessions.get(AGENT).unwrap().closed);
        a.handle(Command::Events(vec![during("SessionStart", json!({ "source": "startup" }))])).await;
        let text = actions_text(fake.actions_for(AGENT).last().unwrap());
        assert!(text.contains("Earlier work on this issue is on branch balerix/gh-acme-api/repo/issue-12; continue from it."), "{text}");
    }

    #[tokio::test]
    async fn phases_update_the_header_and_add_a_line_when_wanted() {
        let (_fake, _port, mut a) = started().await;
        a.handle(Command::Phases(vec![PhaseChange { agent: AGENT.into(), from: balerix_api::AgentPhase::Starting, to: balerix_api::AgentPhase::Ready, message: String::new() }])).await;
        let r = a.statuses[AGENT].render();
        assert!(r.contains("· **ready**") && r.contains("phase **Starting** to **Ready**"), "{r}");
        a.handle(Command::Phases(vec![PhaseChange { agent: "other/x/y".into(), from: balerix_api::AgentPhase::Ready, to: balerix_api::AgentPhase::Dead, message: "gone".into() }])).await;
        assert!(!a.statuses.contains_key("other/x/y"));
    }

    #[tokio::test]
    async fn rows_survive_an_actor_restart_over_the_same_host() {
        let (fake, port, a) = started().await;
        drop(a);
        let host = Host::new(fake.env("github", std::path::Path::new("scratch"))).unwrap();
        let mut b = Actor::new(host, port.clone(), counters(), Health::new(), "balerix".into());
        b.handle(Command::Configure(daemon_config())).await;
        b.load().await;
        assert_eq!(b.sessions.by_number("acme/api", 12), Some(AGENT));
        b.handle(comment(12, "bob", 50, "still here?")).await;
        assert_eq!(fake.actions_for(AGENT).len(), 2, "routed to the reloaded row");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cd plugins/github && mise x -- cargo nextest run actor::tests`
Expected: the new tests FAIL (the stubs do nothing).

- [ ] **Step 3: Implement the stubs**

Replace the Task 7 stubs with:

```rust
    // ---- §8.4 inbound while live ----

    #[allow(clippy::too_many_arguments)]
    async fn on_message(&mut self, agent: &str, installation: u64, repo: &str, number: u64, asker: &str, comment_id: u64, body: &str) {
        self.touch(agent);
        if self.questions.is_open(agent) {
            self.on_answer(agent, installation, repo, number, comment_id, body).await;
            return;
        }
        let action = PluginAction::SendText { text: body.to_string(), submit: true };
        match self.host.action(agent, &action).await {
            Ok(()) => {
                self.counters.inbound.with_label_values(&["routed"]).inc();
                self.react(installation, repo, Target::Comment(comment_id), EYES).await;
                self.track(agent, installation, repo, number, asker, Target::Comment(comment_id), body);
            }
            Err(e) => {
                self.counters.inbound.with_label_values(&["send_failed"]).inc();
                self.counters.errors.with_label_values(&["send_text"]).inc();
                self.post(installation, repo, number, &format!("not delivered to {agent}: {e}"), "notice").await;
                self.react(installation, repo, Target::Comment(comment_id), MINUS_ONE).await;
            }
        }
    }

    /// §8.7: a prose prompt is tracked; a slash command is not.
    #[allow(clippy::too_many_arguments)]
    fn track(&mut self, agent: &str, installation: u64, repo: &str, number: u64, asker: &str, target: Target, body: &str) {
        match delivery::classify(body) {
            BodyKind::Command => { self.counters.deliveries.with_label_values(&["command"]).inc(); }
            BodyKind::Prompt => {
                let window = self.agents.get(agent).map_or(crate::config::DEFAULT_CONFIRM_WINDOW, |c| c.confirm_window);
                let sent = Sent { repo: repo.into(), installation, number, target, agent: agent.into(), asker: asker.into() };
                if let Some(evicted) = self.deliveries.sent(agent, body, sent, Instant::now(), window) {
                    self.unconfirmed_later(evicted);
                }
            }
        }
    }

    fn unconfirmed_later(&mut self, sent: Sent) {
        self.unconfirmed_queue.push(sent);
    }

    async fn unconfirmed(&mut self, sent: Sent) {
        self.counters.deliveries.with_label_values(&["unconfirmed"]).inc();
        self.react(sent.installation, &sent.repo, sent.target, CONFUSED).await;
        let window = self.agents.get(&sent.agent).map_or(crate::config::DEFAULT_CONFIRM_WINDOW, |c| c.confirm_window);
        self.line(&sent.agent, &format!("prompt from @{} not confirmed after {}s", sent.asker, window.as_secs()));
    }

    async fn on_answer(&mut self, agent: &str, installation: u64, repo: &str, number: u64, comment_id: u64, body: &str) {
        use balerix_plugin_common::answer::{self, Reaction};
        use balerix_plugin_common::pending::Stage;
        let Some(open) = self.questions.get(agent).cloned() else { return };
        let delay_ms = self.agents.get(agent).map_or(balerix_api::DEFAULT_KEY_DELAY_MS, |c| c.key_delay_ms);
        let d = answer::on_reply(&open, body, delay_ms);
        // The executor contract of `Decision` (Spec K §4), with comments as
        // posts and reactions on the operator's comment.
        let mut echo = None;
        if let Some(text) = &d.post {
            match self.post(installation, repo, number, text, "question").await {
                Some(id) => echo = Some(id.to_string()),
                None if d.gates_on_post() => {
                    self.counters.inbound.with_label_values(&["send_failed"]).inc();
                    self.questions.set_stage(agent, Stage::Open);
                    self.react(installation, repo, Target::Comment(comment_id), MINUS_ONE).await;
                    return;
                }
                None => {}
            }
        }
        if let Some(action) = &d.send && let Err(e) = self.host.action(agent, action).await {
            self.counters.inbound.with_label_values(&["send_failed"]).inc();
            self.counters.errors.with_label_values(&["send_keys"]).inc();
            self.questions.set_stage(agent, Stage::Open);
            self.post(installation, repo, number, &format!("not delivered to {agent}: {e}"), "notice").await;
            self.react(installation, repo, Target::Comment(comment_id), MINUS_ONE).await;
            return;
        }
        let stage = if d.gates_on_post() { d.stage.with_echo(echo) } else { d.stage };
        self.questions.set_stage(agent, stage);
        if let Some(outcome) = d.outcome {
            self.counters.inbound.with_label_values(&[outcome]).inc();
        }
        if let Some(r) = d.react {
            let content = match r { Reaction::Ack => PLUS_ONE, Reaction::Refused => CONFUSED, Reaction::Failed => MINUS_ONE, Reaction::Confirmed => HOORAY };
            self.react(installation, repo, Target::Comment(comment_id), content).await;
        }
    }

    // ---- §8.5 outbound ----

    async fn on_event(&mut self, event: HookEvent) {
        let Some(config) = self.agents.get(&event.agent).cloned() else { return };
        let Some(row) = self.sessions.get(&event.agent).cloned() else { return };
        self.touch(&event.agent);
        // §8.7 and J §5: both run above every early return.
        let outcome = self.deliveries.on_event(&event, Instant::now());
        for s in outcome.skipped { self.unconfirmed(s).await; }
        if let Some(s) = outcome.confirmed {
            self.counters.deliveries.with_label_values(&["confirmed"]).inc();
            self.react(s.installation, &s.repo, s.target, PLUS_ONE).await;
        }
        let tracking = self.track_question(&event).await;
        if !config.enabled { return; }
        let (installation, repo, number) = (row.installation, row.repo.clone(), row.number);

        if let Some(session) = &event.session_id && row.session_id.as_deref() != Some(session) {
            if let Some(r) = self.sessions.get_mut(&event.agent) { r.session_id = Some(session.clone()); }
            self.rows_dirty.insert(event.agent.clone(), ());
            self.statuses.entry(event.agent.clone()).or_insert_with(|| Status::new(&event.agent)).session = Some(session.clone());
        }
        match event.name.as_str() {
            "SessionStart" => {
                let source = event.payload.get("source").and_then(Value::as_str).unwrap_or("unknown");
                self.line(&event.agent, &format!("session started ({source})"));
                if let Some(fp) = self.first_prompts.remove(&event.agent) {
                    self.send_first_prompt(&event.agent, installation, &repo, number, row.kind, fp).await;
                }
                return;
            }
            "SessionEnd" => {
                self.line(&event.agent, &render::event_message(&event));
                return;
            }
            "Stop" => {
                let message = event.payload.get("last_assistant_message").and_then(Value::as_str).unwrap_or("").trim().to_string();
                if message.is_empty() {
                    self.line(&event.agent, "turn finished");
                } else {
                    self.post(installation, &repo, number, &message, "turn").await;
                    if config.wants("Stop") { self.line(&event.agent, "turn finished"); }
                }
                return;
            }
            _ => {}
        }
        if self.on_question_event(&config, installation, &repo, number, &event, tracking).await { return; }
        if config.wants(&event.name) {
            self.line(&event.agent, &render::event_message(&event));
        }
    }

    async fn send_first_prompt(&mut self, agent: &str, installation: u64, repo: &str, number: u64, kind: Kind, fp: FirstPrompt) {
        let s = prompt::Start { repo, kind, number, title: &fp.title, url: &fp.url, body: &fp.body, branch: &fp.branch, base: &fp.base, asker: &fp.asker, comment: fp.comment.as_deref(), resumed: fp.resumed };
        let text = prompt::first(&s);
        match self.host.action(agent, &PluginAction::SendText { text: text.clone(), submit: true }).await {
            Ok(()) => self.track(agent, installation, repo, number, &fp.asker, fp.target, &text),
            Err(e) => {
                self.counters.errors.with_label_values(&["send_text"]).inc();
                self.post(installation, repo, number, &format!("not delivered to {agent}: {e}"), "notice").await;
                self.react(installation, repo, fp.target, MINUS_ONE).await;
            }
        }
    }

    /// Matrix's `track_question`, verbatim in shape (Spec J §5, §7.1).
    async fn track_question(&mut self, event: &HookEvent) -> Tracking {
        use balerix_plugin_common::question;
        if matches!(event.name.as_str(), "Stop" | "UserPromptSubmit" | "SessionStart" | "SessionEnd") {
            self.questions.clear(&self.host, &event.agent).await;
            if event.name == "Stop" || event.name == "UserPromptSubmit" { self.release_held(&event.agent).await; }
            return Tracking::Other;
        }
        if event.payload.get("tool_name").and_then(Value::as_str) != Some(question::TOOL) { return Tracking::Other; }
        match event.name.as_str() {
            "PreToolUse" => {
                let input = event.payload.get("tool_input").cloned().unwrap_or(Value::Null);
                match question::parse(&input) {
                    Some(qs) => { self.questions.open(&self.host, &event.agent, &input, qs.clone()).await; Tracking::Opened(qs) }
                    None => Tracking::Unparsed,
                }
            }
            "PostToolUse" => {
                let closed = self.questions.clear(&self.host, &event.agent).await;
                self.release_held(&event.agent).await;
                Tracking::Closed(closed)
            }
            _ => Tracking::Other,
        }
    }

    /// Matrix's `on_question_event`, with comments for posts and
    /// reactions on the echo. `true` when handled here.
    async fn on_question_event(&mut self, config: &AgentConfig, installation: u64, repo: &str, number: u64, event: &HookEvent, tracking: Tracking) -> bool {
        use balerix_plugin_common::answer::{self, Verdict};
        match tracking {
            Tracking::Opened(qs) => {
                let body = render::question_message(&qs).replace("Reply with a number or a label.", "Reply with a comment: a number or a label.").replace("Reply with one line per question, in order: a number or a label.", "Reply with a comment, one line per question, in order: a number or a label.");
                if self.post(installation, repo, number, &body, "question").await.is_some() { self.questions.mark_posted(&event.agent); }
                true
            }
            Tracking::Unparsed | Tracking::Closed(None) => false,
            Tracking::Closed(Some(open)) => {
                let answers = event.payload.pointer("/tool_response/answers").cloned().unwrap_or(Value::Null);
                match answer::on_closed(&open, &answers) {
                    Verdict::Confirmed { echo } => {
                        if let Some(id) = echo.and_then(|e| e.parse::<u64>().ok()) { self.react(installation, repo, Target::Comment(id), HOORAY).await; }
                    }
                    Verdict::Mismatch { message } => { self.counters.answers_mismatched.inc(); self.post(installation, repo, number, &message, "question").await; }
                    Verdict::AnsweredAtTerminal { message } => { self.post(installation, repo, number, &message, "question").await; }
                    Verdict::Nothing => {}
                }
                true
            }
            Tracking::Other => {
                let _ = config;
                event.name == "Notification"
                    && event.payload.get("notification_type").and_then(Value::as_str) == Some("permission_prompt")
                    && self.questions.suppress_permission_prompt(&event.agent)
            }
        }
    }

    // ---- §9 reviews ----

    #[allow(clippy::too_many_arguments)]
    async fn on_review(&mut self, installation: u64, repo: &str, number: u64, author: Author, review_id: u64, state: &str, body: &str, commit: &str) {
        let Some(agent) = self.sessions.by_number(repo, number).map(str::to_string) else { return };
        let Some(row) = self.sessions.get(&agent).cloned() else { return };
        if row.closed || row.kind != Kind::Pr || !self.permitted(installation, repo, &author).await { return; }
        let comments = match retry_once(|| self.port.review_comments(installation, repo, number, review_id)).await {
            Ok(c) => c,
            Err(e) => { self.github_failed("review_comments", &e); return; }
        };
        let base = self.first_prompts.get(&agent).map(|f| f.base.clone()).unwrap_or_else(|| "main".into());
        let Some(message) = prompt::review(&author.login, state, commit, body, &comments, &base) else { return };
        if self.questions.is_open(&agent) {
            self.held.entry(agent.clone()).or_default().push(Held { reviewer: author.login.clone(), message });
            self.line(&agent, &format!("review from @{} held", author.login));
            return;
        }
        self.deliver_review(&agent, &author.login, message).await;
    }

    async fn deliver_review(&mut self, agent: &str, reviewer: &str, message: String) {
        match self.host.action(agent, &PluginAction::SendText { text: message, submit: true }).await {
            Ok(()) => self.line(agent, &format!("review from @{reviewer} delivered")),
            Err(e) => { self.counters.errors.with_label_values(&["send_text"]).inc(); self.line(agent, &format!("review from @{reviewer} not delivered: {e}")); }
        }
    }

    async fn release_held(&mut self, agent: &str) {
        if self.questions.is_open(agent) { return; }
        for h in self.held.remove(agent).unwrap_or_default() {
            self.deliver_review(agent, &h.reviewer, h.message).await;
        }
    }

    // ---- §10 ending ----

    async fn end(&mut self, agent: &str, installation: u64, reason: &str) {
        let Some(row) = self.sessions.get(agent).cloned() else { return };
        if row.closed { return; }
        let fleet = session::fleet_of(agent).to_string();
        let mut live = self.sessions.live_in(&fleet);
        live.retain(|l| l.number != row.number);
        let refile = async {
            let default_branch = retry_once(|| self.port.default_branch(installation, &row.repo)).await?;
            let text = retry_once(|| self.port.read_file(installation, &row.repo, &self.cfg().config_path, &default_branch)).await?.unwrap_or_default();
            Ok::<_, GitHubError>(repo_config::prepare(&text, &row.repo, &fleet, &default_branch, &live))
        };
        match refile.await {
            Ok(Ok(file)) => {
                if let Err(e) = self.host.apply_fleet(&fleet, &file).await { tracing::warn!("github: applying {fleet} without {agent}: {e}"); self.counters.applies.with_label_values(&["daemon"]).inc(); }
                else { self.counters.applies.with_label_values(&["ok"]).inc(); }
            }
            Ok(Err(m)) => { tracing::warn!("github: {fleet}: {m}"); self.counters.applies.with_label_values(&["config"]).inc(); }
            Err(e) => self.github_failed("read_file", &e),
        }
        self.line(agent, reason);
        if let Some(r) = self.sessions.get_mut(agent) { r.closed = true; }
        self.rows_dirty.insert(agent.to_string(), ());
        self.deliveries.forget(agent);
        self.held.remove(agent);
        self.questions.clear(&self.host, agent).await;
        self.publish_gauges();
    }

    // ---- the tick ----

    async fn on_tick(&mut self) {
        for s in std::mem::take(&mut self.unconfirmed_queue) { self.unconfirmed(s).await; }
        for s in self.deliveries.expire(Instant::now()) { self.unconfirmed(s).await; }
        // idle (§10)
        let idle = self.cfg().idle_timeout;
        if !idle.is_zero() {
            let now = (self.now_secs)();
            let stale: Vec<(String, u64)> = self.sessions.agents().into_iter().filter_map(|a| {
                let r = self.sessions.get(&a)?;
                (!r.closed && now.saturating_sub(r.last_activity) > idle.as_secs()).then(|| (a, r.installation))
            }).collect();
            for (agent, installation) in stale {
                let h = idle.as_secs() / 3600;
                let reason = format!("stopped after {} idle — mention @{} to resume", if h > 0 { format!("{h}h") } else { format!("{}m", idle.as_secs() / 60) }, self.slug);
                self.end(&agent, installation, &reason).await;
            }
        }
        // status flushes (§8.3)
        let due: Vec<String> = self.status_dirty.iter().filter(|(_, at)| Instant::now().duration_since(**at) >= STATUS_COALESCE).map(|(a, _)| a.clone()).collect();
        for agent in due { self.flush_status(&agent).await; }
        // row write-back (§8.4)
        for agent in std::mem::take(&mut self.rows_dirty).into_keys() {
            if let Some(row) = self.sessions.get(&agent).cloned() && let Err(e) = self.sessions.set(&self.host, &agent, row).await {
                tracing::warn!("github: writing row for {agent}: {e}");
            }
        }
    }

    // ---- §8.3 the status comment ----

    fn touch(&mut self, agent: &str) {
        if let Some(r) = self.sessions.get_mut(agent) { r.last_activity = (self.now_secs)(); }
        self.rows_dirty.insert(agent.to_string(), ());
    }

    fn line(&mut self, agent: &str, text: &str) {
        let at = (self.now_secs)();
        self.statuses.entry(agent.to_string()).or_insert_with(|| Status::new(agent)).push(at, text);
        self.status_dirty.entry(agent.to_string()).or_insert_with(Instant::now);
    }

    async fn flush_status(&mut self, agent: &str) {
        self.status_dirty.remove(agent);
        let (Some(row), Some(status)) = (self.sessions.get(agent).cloned(), self.statuses.get(agent)) else { return };
        let body = status.render();
        let edited = match row.status_comment {
            Some(id) => match retry_once(|| self.port.edit_comment(row.installation, &row.repo, id, &body)).await {
                Ok(()) => { self.counters.status_edits.with_label_values(&["ok"]).inc(); return; }
                Err(GitHubError::NotFound) => false,
                Err(e) => { self.counters.status_edits.with_label_values(&["failed"]).inc(); self.github_failed("edit_comment", &e); self.status_dirty.insert(agent.into(), Instant::now()); return; }
            },
            None => false,
        };
        debug_assert!(!edited);
        match retry_once(|| self.port.comment(row.installation, &row.repo, row.number, &body)).await {
            Ok(id) => {
                self.counters.status_edits.with_label_values(&["reposted"]).inc();
                if let Some(r) = self.sessions.get_mut(agent) { r.status_comment = Some(id); }
                self.rows_dirty.insert(agent.into(), ());
            }
            Err(e) => self.github_failed("comment", &e),
        }
    }

    async fn on_phase(&mut self, change: PhaseChange) {
        let Some(config) = self.agents.get(&change.agent).cloned() else { return };
        if self.sessions.get(&change.agent).is_none() { return; }
        let phase = format!("{:?}", change.to).to_lowercase();
        self.statuses.entry(change.agent.clone()).or_insert_with(|| Status::new(&change.agent)).phase = phase;
        if config.phases {
            self.line(&change.agent, &render::phase_message(&change));
        } else {
            self.status_dirty.entry(change.agent.clone()).or_insert_with(Instant::now);
        }
    }
```

with `enum Tracking { Other, Opened(Vec<balerix_plugin_common::question::Question>), Unparsed, Closed(Option<balerix_plugin_common::pending::OpenQuestion>) }` and the two new fields `unconfirmed_queue: Vec<Sent>` and `first_prompts` on the struct. In `start`, when the row existed and was closed, set `closed = false` on the new row (the resume case) and keep `status_comment` from the old row so the same comment continues.

The echo id: common's `Stage::with_echo` takes `Option<String>`; here the comment id is stored as its decimal string and parsed back for the `hooray` reaction (`Verdict::Confirmed { echo }`).

- [ ] **Step 4: Run, review the first-prompt snapshot, commit**

Run: `cd plugins/github && mise x -- cargo nextest run actor`, read the `.snap.new` for the first prompt against Spec M §8.2, `mise x -- cargo insta accept`, then `mise run plugin github`.
Expected: PASS.

```bash
git add plugins/github
git commit -m "feat(github): turns, questions, replies, reviews, ending and the status comment (Spec M §8.3-§10)"
```

---

### Task 9: `client.rs`, the `reqwest` implementation

**Files:**
- Create: `plugins/github/src/client.rs`.

**Interfaces:**
- Produces: `client::GitHubClient::new(app_id: u64, private_key: &Secret) -> Result<Self, String>` implementing `GitHubPort`; `client::API: &str = "https://api.github.com"`; `client::jwt(app_id, key: &Secret, now: u64) -> Result<String, String>` (pure, testable); `client::retry_after_ms(status: u16, headers: &reqwest::header::HeaderMap, now: u64) -> Option<u64>` (pure).

- [ ] **Step 1: Write the pure tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // A throwaway 2048-bit key generated for this test only:
    // `openssl genrsa 2048`. Not a secret; it signs nothing real.
    const KEY: &str = include_str!("../tests/fixtures/test-app-key.pem");

    #[test]
    fn the_app_jwt_carries_iss_iat_and_a_ten_minute_exp() {
        let token = jwt(12345, &Secret::new(KEY), 1_700_000_000).unwrap();
        let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        v.validate_exp = false;
        v.set_required_spec_claims::<&str>(&[]);
        let key = jsonwebtoken::DecodingKey::from_rsa_pem(pub_key_of(KEY).as_bytes()).unwrap();
        let data = jsonwebtoken::decode::<Claims>(&token, &key, &v).unwrap();
        assert_eq!(data.claims.iss, "12345");
        assert_eq!(data.claims.iat, 1_700_000_000 - 60);
        assert_eq!(data.claims.exp, 1_700_000_000 + 540);
        assert!(jwt(1, &Secret::new("not a key"), 0).unwrap_err().contains("privateKey"));
    }

    #[test]
    fn retry_after_reads_the_header_then_the_reset_epoch() {
        let mut h = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after_ms(200, &h, 0), None);
        assert_eq!(retry_after_ms(403, &h, 0), None, "a plain 403 is not a rate limit");
        h.insert("retry-after", "2".parse().unwrap());
        assert_eq!(retry_after_ms(429, &h, 0), Some(2000));
        h.remove("retry-after");
        h.insert("x-ratelimit-remaining", "0".parse().unwrap());
        h.insert("x-ratelimit-reset", "1000".parse().unwrap());
        assert_eq!(retry_after_ms(403, &h, 997), Some(3000));
        assert_eq!(retry_after_ms(403, &h, 5000), Some(0));
    }
}
```

`tests/fixtures/test-app-key.pem` is generated once with `openssl genrsa 2048 > plugins/github/tests/fixtures/test-app-key.pem` and committed; add a `plugins/github/tests/fixtures/README` line saying it is a throwaway test key (gitleaks: add its path to `.gitleaks.toml`'s allowlist with the same comment). `pub_key_of` derives the public PEM in the test with `openssl rsa -pubout` output committed beside it as `test-app-key.pub.pem` (`include_str!`).

- [ ] **Step 2: Implement**

```rust
//! The GitHub REST client (M-12): App JWT → installation token per
//! installation (cached until a minute before expiry), the endpoints
//! `GitHubPort` names, one retry on `Retry-After`, a 401 on an
//! installation token refetched once.

use std::collections::HashMap;
use std::sync::Mutex;

use balerix_plugin_common::config::Secret;
use base64::Engine;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::github::{GitHubError, GitHubPort, IssueInfo, Permission, ReviewComment, Target};

pub const API: &str = "https://api.github.com";

#[derive(Debug, serde::Serialize, Deserialize)]
pub(crate) struct Claims {
    pub iss: String,
    pub iat: u64,
    pub exp: u64,
}

/// RS256, `iat` a minute back for clock skew, `exp` nine minutes ahead
/// (GitHub allows ten).
pub fn jwt(app_id: u64, key: &Secret, now: u64) -> Result<String, String> {
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(key.expose().as_bytes())
        .map_err(|e| format!("privateKey: not an RSA PEM: {e}"))?;
    let claims = Claims { iss: app_id.to_string(), iat: now - 60, exp: now + 540 };
    jsonwebtoken::encode(&jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256), &claims, &key)
        .map_err(|e| format!("privateKey: signing: {e}"))
}

/// GitHub's two ways of saying "later": `Retry-After` seconds, or
/// `x-ratelimit-remaining: 0` with a reset epoch, on 403 or 429.
pub fn retry_after_ms(status: u16, headers: &HeaderMap, now: u64) -> Option<u64> {
    if status != 403 && status != 429 {
        return None;
    }
    let get = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<u64>().ok());
    if let Some(s) = get("retry-after") {
        return Some(s * 1000);
    }
    if get("x-ratelimit-remaining") == Some(0) && let Some(reset) = get("x-ratelimit-reset") {
        return Some(reset.saturating_sub(now) * 1000);
    }
    None
}

struct Token {
    value: Secret,
    expires: u64,
}

pub struct GitHubClient {
    http: reqwest::Client,
    app_id: u64,
    key: Secret,
    tokens: Mutex<HashMap<u64, Token>>,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl GitHubClient {
    pub fn new(app_id: u64, key: &Secret) -> Result<Self, String> {
        jwt(app_id, key, now())?; // proves the PEM parses before any request
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/vnd.github+json"));
        headers.insert("X-GitHub-Api-Version", HeaderValue::from_static("2022-11-28"));
        headers.insert(USER_AGENT, HeaderValue::from_str(&format!("balerix-plugin-github/{}", env!("CARGO_PKG_VERSION"))).map_err(|e| e.to_string())?);
        let http = reqwest::Client::builder().default_headers(headers).timeout(std::time::Duration::from_secs(20)).build().map_err(|e| e.to_string())?;
        Ok(Self { http, app_id, key: key.clone(), tokens: Mutex::new(HashMap::new()) })
    }

    async fn installation_token(&self, installation: u64, force: bool) -> Result<Secret, GitHubError> {
        if !force && let Some(t) = self.tokens.lock().unwrap_or_else(|e| e.into_inner()).get(&installation) && t.expires > now() + 60 {
            return Ok(t.value.clone());
        }
        let jwt = jwt(self.app_id, &self.key, now()).map_err(GitHubError::Auth)?;
        let resp = self.http.post(format!("{API}/app/installations/{installation}/access_tokens")).header(AUTHORIZATION, format!("Bearer {jwt}")).send().await.map_err(|e| GitHubError::Other(e.to_string()))?;
        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| GitHubError::Other(e.to_string()))?;
        if !status.is_success() {
            return Err(GitHubError::Auth(format!("installation token: HTTP {}: {}", status.as_u16(), body["message"].as_str().unwrap_or(""))));
        }
        let value = Secret::new(body["token"].as_str().unwrap_or(""));
        // `expires_at` is RFC 3339; an hour is GitHub's fixed lifetime, so
        // a parse is not worth a dependency: cache for fifty minutes.
        self.tokens.lock().unwrap_or_else(|e| e.into_inner()).insert(installation, Token { value: value.clone(), expires: now() + 50 * 60 });
        Ok(value)
    }

    /// One request with the installation token; a 401 drops the token
    /// and retries once (§11); 403/429 with a retry hint is `RateLimited`;
    /// 404 is `NotFound`.
    async fn call(&self, installation: u64, method: Method, path: &str, body: Option<Value>) -> Result<Value, GitHubError> {
        let mut refreshed = false;
        loop {
            let token = self.installation_token(installation, refreshed).await?;
            let mut req = self.http.request(method.clone(), format!("{API}{path}")).header(AUTHORIZATION, format!("Bearer {}", token.expose()));
            if let Some(b) = &body { req = req.json(b); }
            let resp = req.send().await.map_err(|e| GitHubError::Other(e.to_string()))?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                continue;
            }
            if let Some(ms) = retry_after_ms(status.as_u16(), resp.headers(), now()) {
                return Err(GitHubError::RateLimited { retry_after_ms: ms });
            }
            if status == StatusCode::NOT_FOUND { return Err(GitHubError::NotFound); }
            if status == StatusCode::UNAUTHORIZED { return Err(GitHubError::Auth("installation token rejected twice".into())); }
            let text = resp.text().await.map_err(|e| GitHubError::Other(e.to_string()))?;
            if !status.is_success() {
                let message = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["message"].as_str().map(str::to_string)).unwrap_or_else(|| text.chars().take(200).collect());
                return Err(GitHubError::Other(format!("{method} {path}: HTTP {}: {message}", status.as_u16())));
            }
            if text.trim().is_empty() { return Ok(Value::Null); }
            return serde_json::from_str(&text).map_err(|e| GitHubError::Other(format!("{method} {path}: bad JSON: {e}")));
        }
    }
}

fn split_repo(repo: &str) -> (&str, &str) {
    repo.split_once('/').unwrap_or((repo, ""))
}

impl GitHubPort for GitHubClient {
    async fn app_slug(&self) -> Result<String, GitHubError> {
        let jwt = jwt(self.app_id, &self.key, now()).map_err(GitHubError::Auth)?;
        let resp = self.http.get(format!("{API}/app")).header(AUTHORIZATION, format!("Bearer {jwt}")).send().await.map_err(|e| GitHubError::Other(e.to_string()))?;
        let status = resp.status();
        let v: Value = resp.json().await.map_err(|e| GitHubError::Other(e.to_string()))?;
        if !status.is_success() { return Err(GitHubError::Auth(format!("GET /app: HTTP {}: {}", status.as_u16(), v["message"].as_str().unwrap_or("")))); }
        v["slug"].as_str().map(str::to_string).ok_or_else(|| GitHubError::Other("GET /app: no slug".into()))
    }
    async fn default_branch(&self, installation: u64, repo: &str) -> Result<String, GitHubError> {
        let v = self.call(installation, Method::GET, &format!("/repos/{repo}"), None).await?;
        Ok(v["default_branch"].as_str().unwrap_or("main").to_string())
    }
    async fn read_file(&self, installation: u64, repo: &str, path: &str, git_ref: &str) -> Result<Option<String>, GitHubError> {
        let v = match self.call(installation, Method::GET, &format!("/repos/{repo}/contents/{path}?ref={git_ref}"), None).await {
            Ok(v) => v,
            Err(GitHubError::NotFound) => return Ok(None),
            Err(e) => return Err(e),
        };
        let content: String = v["content"].as_str().unwrap_or("").chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = base64::engine::general_purpose::STANDARD.decode(content).map_err(|e| GitHubError::Other(format!("contents of {path}: {e}")))?;
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }
    async fn permission(&self, installation: u64, repo: &str, login: &str) -> Result<Permission, GitHubError> {
        let v = self.call(installation, Method::GET, &format!("/repos/{repo}/collaborators/{login}/permission"), None).await?;
        Ok(Permission::parse(v["role_name"].as_str(), v["permission"].as_str()))
    }
    async fn issue(&self, installation: u64, repo: &str, number: u64) -> Result<IssueInfo, GitHubError> {
        let v = self.call(installation, Method::GET, &format!("/repos/{repo}/issues/{number}"), None).await?;
        let pr = if v.get("pull_request").is_some() {
            let p = self.call(installation, Method::GET, &format!("/repos/{repo}/pulls/{number}"), None).await?;
            Some((p["head"]["ref"].as_str().unwrap_or("").into(), p["head"]["repo"]["full_name"].as_str().unwrap_or("").into(), p["base"]["ref"].as_str().unwrap_or("").into()))
        } else { None };
        Ok(IssueInfo { title: v["title"].as_str().unwrap_or("").into(), body: v["body"].as_str().unwrap_or("").into(), url: v["html_url"].as_str().unwrap_or("").into(), pr })
    }
    async fn comment(&self, installation: u64, repo: &str, number: u64, body: &str) -> Result<u64, GitHubError> {
        let v = self.call(installation, Method::POST, &format!("/repos/{repo}/issues/{number}/comments"), Some(json!({ "body": body }))).await?;
        v["id"].as_u64().ok_or_else(|| GitHubError::Other("comment: no id".into()))
    }
    async fn edit_comment(&self, installation: u64, repo: &str, comment_id: u64, body: &str) -> Result<(), GitHubError> {
        self.call(installation, Method::PATCH, &format!("/repos/{repo}/issues/comments/{comment_id}"), Some(json!({ "body": body }))).await.map(|_| ())
    }
    async fn react(&self, installation: u64, repo: &str, target: Target, content: &str) -> Result<(), GitHubError> {
        let path = match target {
            Target::Issue(n) => format!("/repos/{repo}/issues/{n}/reactions"),
            Target::Comment(id) => format!("/repos/{repo}/issues/comments/{id}/reactions"),
        };
        self.call(installation, Method::POST, &path, Some(json!({ "content": content }))).await.map(|_| ())
    }
    async fn review_comments(&self, installation: u64, repo: &str, number: u64, review_id: u64) -> Result<Vec<ReviewComment>, GitHubError> {
        let v = self.call(installation, Method::GET, &format!("/repos/{repo}/pulls/{number}/reviews/{review_id}/comments?per_page=100"), None).await?;
        serde_json::from_value(v).map_err(|e| GitHubError::Other(format!("review comments: {e}")))
    }
}
```

(`split_repo` is unused if every path interpolates `repo` whole, as above; drop it.) `GitHubLauncher` (Task 10) lives here too.

- [ ] **Step 3: Run and commit**

Add `pub mod client;` to `lib.rs`. Run: `mise run plugin github`
Expected: PASS.

```bash
git add plugins/github .gitleaks.toml
git commit -m "feat(github): the reqwest client, App JWT and installation tokens (M-12)"
```

---

### Task 10: `plugin.rs`, the launcher, `main.rs` and the integration test

**Files:**
- Create: `plugins/github/src/plugin.rs`, `plugins/github/tests/plugin_it.rs`.
- Modify: `plugins/github/src/client.rs` (`GitHubLauncher`), `src/main.rs`.

**Interfaces:**
- Produces:
  - `plugin::Launcher` trait: `fn launch(&self, config: DaemonConfig, queue: Arc<Queue>) -> impl Future<Output = Result<(), String>> + Send`.
  - `plugin::GitHubPlugin<L: Launcher>`: `new(metrics, health, queue, launcher)`, `queue()`; `Plugin` impl as matrix's (`configure` parses, launches, pushes `Configure`; `activate` parses and pushes; `deactivate`; `observe`; `health`; `metrics`).
  - `client::GitHubLauncher { host: Host, counters: Counters, health: Health }`: builds `GitHubClient`, proves the App (`app_slug`; `Err` fails `configure`), starts the actor with the slug, binds `config.listen` and serves the webhook router into the queue, spawns the ticker.

- [ ] **Step 1: `plugin.rs`**

Matrix's `plugin.rs` with the names changed (`GitHubPlugin`, `crate::config::{parse_agent, parse_daemon}`, `crate::actor::{Command, Health, Queue}`) and its unit tests (a `FakeLauncher`, `the_wire_surface_works_end_to_end` over `Harness` with `daemon_json()` = `{ "appId": 1, "privateKey": "k", "webhookSecret": "s" }`, and an `activate` of `{ "kind": "issue", "number": 12 }` accepted and of `{}` rejected with `number: missing`).

- [ ] **Step 2: The launcher and `main.rs`**

In `client.rs`:

```rust
pub struct GitHubLauncher {
    pub host: Host,
    pub counters: Counters,
    pub health: Health,
}

impl crate::plugin::Launcher for GitHubLauncher {
    async fn launch(&self, config: DaemonConfig, queue: Arc<Queue>) -> Result<(), String> {
        let client = GitHubClient::new(config.app_id, &config.private_key)?;
        let slug = client.app_slug().await.map_err(|e| format!("proving the App: {e}"))?;
        tracing::info!("github: App @{slug}");
        let listener = tokio::net::TcpListener::bind(config.listen).await.map_err(|e| format!("listen {}: {e}", config.listen))?;
        let mut actor = Actor::new(self.host.clone(), client, self.counters.clone(), self.health.clone(), slug);
        actor.load().await;
        tokio::spawn(actor.run(queue.clone()));
        let sink_queue = queue.clone();
        let webhook = Listener::new(config.webhook_secret.clone(), move |ev| sink_queue.push(Command::Webhook(ev)), self.counters.webhooks.clone());
        let health = self.health.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::webhook::serve(listener, webhook.router()).await {
                health.fail(format!("webhook listener: {e}"));
            }
        });
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(TICK);
            loop {
                interval.tick().await;
                queue.push(Command::Tick);
            }
        });
        Ok(())
    }
}
```

`main.rs`: matrix's, with `github`, `GitHubPlugin`, `GitHubLauncher`; the `phases::run` spawn stays.

- [ ] **Step 3: `tests/plugin_it.rs`**

```rust
//! The real `Plugin` over the wire: `Harness` drives `hello`/`activate`/
//! `observe`, a signed webhook hits the listener, `FakeHost` records the
//! apply and the `send_text`, `FakePort` the comments and reactions.

use std::sync::Arc;
use std::time::Duration;

use balerix_api::PluginAction;
use balerix_plugin_github::actor::{Actor, Command, Counters, Health, Queue};
use balerix_plugin_github::config::DaemonConfig;
use balerix_plugin_github::github::fake::{Call, FakePort};
use balerix_plugin_github::github::Permission;
use balerix_plugin_github::webhook::Listener;
use balerix_plugin_github::{GitHubPlugin, Launcher};
use balerix_plugin_sdk::testing::{FakeHost, Harness, event};
use balerix_plugin_sdk::{Host, Metrics};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;

struct TestLauncher { host: Host, port: FakePort, counters: Counters, health: Health, listen: Arc<std::sync::Mutex<Option<String>>> }

impl Launcher for TestLauncher {
    async fn launch(&self, config: DaemonConfig, queue: Arc<Queue>) -> Result<(), String> {
        let mut actor = Actor::new(self.host.clone(), self.port.clone(), self.counters.clone(), self.health.clone(), "balerix".into());
        actor.load().await;
        tokio::spawn(actor.run(queue.clone()));
        let listener = tokio::net::TcpListener::bind(config.listen).await.map_err(|e| e.to_string())?;
        *self.listen.lock().unwrap() = Some(listener.local_addr().unwrap().to_string());
        let q = queue.clone();
        let l = Listener::new(config.webhook_secret.clone(), move |ev| q.push(Command::Webhook(ev)), self.counters.webhooks.clone());
        tokio::spawn(balerix_plugin_github::webhook::serve(listener, l.router()));
        Ok(())
    }
}

async fn eventually(label: &str, mut done: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(5), "timed out waiting for {label}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    format!("sha256={}", mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mention_starts_a_session_and_the_first_prompt_reaches_the_agent() {
    let config = json!({ "appId": 1, "privateKey": "k", "webhookSecret": "s3cret", "listen": "127.0.0.1:0" });
    let fake = Arc::new(FakeHost::start("tok", config, Vec::new()).await);
    let env = fake.env("github", std::path::Path::new("scratch"));
    let host = Host::new(env.clone()).unwrap();
    let metrics = Metrics::new("github");
    let counters = Counters::new(&metrics).unwrap();
    let health = Health::new();
    let queue = Queue::new(counters.events_dropped.clone());
    let port = FakePort::new("balerix");
    port.set_permission("acme/api", "alice", Permission::Write);
    port.set_file("acme/api", "main", ".balerix.yaml", "apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  repo:\n    repo: acme/api\n");
    let listen_cell = Arc::new(std::sync::Mutex::new(None));
    let launcher = TestLauncher { host, port: port.clone(), counters: counters.clone(), health: Health::new(), listen: listen_cell.clone() };
    let plugin = GitHubPlugin::new(metrics, health, queue.clone(), launcher);
    let h = Harness::start(&env, plugin).await;
    let listen = listen_cell.lock().unwrap().clone().unwrap();

    let payload = json!({ "action": "created", "repository": { "full_name": "acme/api" }, "installation": { "id": 7 }, "issue": { "number": 12 }, "comment": { "id": 5, "body": "@balerix take a look", "user": { "login": "alice", "type": "User" } } });
    let body = serde_json::to_vec(&payload).unwrap();
    let resp = reqwest::Client::new().post(format!("http://{listen}/webhook")).header("X-Hub-Signature-256", sign("s3cret", &body)).header("X-GitHub-Delivery", "d1").header("X-GitHub-Event", "issue_comment").body(body).send().await.unwrap();
    assert_eq!(resp.status(), 202);
    let f = fake.clone();
    eventually("the apply", || !f.applied_fleets().is_empty()).await;
    assert_eq!(fake.applied_fleets()[0].0, "gh-acme-api");
    let p = port.clone();
    eventually("the status comment", || p.calls().iter().any(|c| matches!(c, Call::Comment { .. }))).await;

    h.activate("gh-acme-api/repo/issue-12", json!({ "kind": "issue", "number": 12 })).await.unwrap();
    let mut start = event("gh-acme-api/repo/issue-12", "SessionStart", json!({ "source": "startup" }));
    start.session_id = Some("s1".into());
    h.observe(vec![start]).await;
    let f = fake.clone();
    eventually("the first prompt", || !f.actions_for("gh-acme-api/repo/issue-12").is_empty()).await;
    assert!(matches!(fake.actions_for("gh-acme-api/repo/issue-12")[0], PluginAction::SendText { submit: true, .. }));
    assert!(h.health().await.is_ok());
    assert!(h.metrics().await.contains("balerix_plugin_github_webhooks_total"));
}
```

`FakeHost` is not `Clone`, which is why the test holds it in an `Arc`; `fake.env(…)`, `applied_fleets()` and `actions_for()` are reached through the `Arc` unchanged.

- [ ] **Step 4: Run everything and commit**

Run: `mise run plugin github` and `mise run package-plugins github` (needs Task 11's `package-plugins.sh` default? No: names are explicit here) and `target/plugins/github/bin/balerix-plugin-github` without the daemon's environment.
Expected: PASS; the binary prints `github: environment: BALERIX_API_URL is not set` and exits 1.

```bash
git add plugins/github
git commit -m "feat(github): the plugin surface, the launcher, main, and the wire test (Spec M §3, §11)"
```

---

### Task 11: `verify-github`, the plumbing, and the documentation

**Files:**
- Create: `scripts/verify-github.sh`.
- Modify: `mise.toml:29-36` (`fmt`), `:69-76` (`plugins`), `:89-102` (`audit`), `:146` (`release-prepare` help), plus a `verify-github` task after `verify-matrix`; `scripts/package-plugins.sh:15`; `.github/workflows/ci.yml:49, 57`; `.github/workflows/release-pr.yml:12, 31`; `.github/actions/build-binary/action.yml:8`, `.github/actions/build-image/action.yml:9`; `scripts/release/lib.sh:8, 10, 16, 34`; `scripts/release/test.sh:98, 162, 164, 416-434, 436, 508, 514`; `.vscode/settings.json:3-6`; `docs/RELEASING.md:14, 141, 149`; `AGENTS.md:14, 23-24, 35-42, 49`; `ARCHITECTURE.md:71-79`; `README.md:54-55, 74-77`; `plugins/common/README.md:7, 129-131`; `docs/THREAT-MODEL.md:100` and the three §12 entries; the Spec M file (§18).

- [ ] **Step 1: The manual check**

`scripts/verify-github.sh`, on `scripts/verify-matrix.sh`'s shape:

```bash
#!/usr/bin/env bash
# Spec M's manual check against a real GitHub App on a scratch repository.
# Not part of any CI tier: it needs an App, its key, a repository the App
# is installed on, and a listener GitHub can reach.
#
# Required environment:
#   GITHUB_APP_ID          the App's numeric id
#   GITHUB_APP_KEY         path to the App's private key (PEM)
#   GITHUB_WEBHOOK_SECRET  the webhook secret configured on the App
#   GITHUB_REPO            owner/name of a scratch repository the App is installed on
#   GITHUB_LISTEN          host:port the plugin listens on (default 127.0.0.1:8787);
#                          front it with a tunnel and point the App's webhook URL at
#                          https://<tunnel>/webhook
#
# What it does: starts nothing; writes the daemon config under
# target/tmp/verify-github and prints what to do and look for.
set -euo pipefail
cd "$(dirname "$0")/.."

for var in GITHUB_APP_ID GITHUB_APP_KEY GITHUB_WEBHOOK_SECRET GITHUB_REPO; do
  if [[ -z "${!var:-}" ]]; then
    echo "verify-github: $var is not set" >&2
    exit 1
  fi
done
listen="${GITHUB_LISTEN:-127.0.0.1:8787}"

root="$PWD/target/tmp/verify-github"
rm -rf "$root"
mkdir -p "$root/config/balerix" "$root/secrets"
cp "$GITHUB_APP_KEY" "$root/secrets/github-app.pem"
printf '%s' "$GITHUB_WEBHOOK_SECRET" > "$root/secrets/github-webhook"
chmod 600 "$root/secrets/github-app.pem" "$root/secrets/github-webhook"

mise run package-plugins github

cat > "$root/config/balerix/plugins.yaml" <<YAML
plugins:
  - name: github
    source: $PWD/target/plugins/github
    secrets:
      privateKey: $root/secrets/github-app.pem
      webhookSecret: $root/secrets/github-webhook
    config:
      appId: $GITHUB_APP_ID
      listen: "$listen"
      idleTimeout: 3m
YAML

cat <<NOTES
verify-github: config written. Now, by hand:

  1. Push a .balerix.yaml to $GITHUB_REPO's default branch:

       apiVersion: balerix/v1
       kind: Fleet
       crews:
         repo:
           repo: $GITHUB_REPO

  2. Start the daemon with XDG_CONFIG_HOME, XDG_STATE_HOME, XDG_DATA_HOME
     and HOME pointed under target/tmp/verify-github, as \`mise run serve\`
     does; confirm \`balerix plugin list\` shows github ready, and that the
     App's webhook URL reaches $listen (GitHub's "Recent Deliveries" shows
     a 200 on the ping).

Then check, on $GITHUB_REPO:

  - open an issue and comment "@<app slug> please summarise this issue":
    the comment gets eyes, a status comment appears and turns "ready", the
    first prompt reaches Claude (the status line "session started"), the
    comment gains +1 when Claude takes it, and Claude's turn appears as a
    comment;
  - a further comment from you (write permission) reaches the agent with
    eyes then +1; a comment from an account without write does nothing;
  - ask the agent to use AskUserQuestion: the question posts as a comment,
    a comment answering with a number gets +1 and an "answering" echo, and
    the echo gains hooray when Claude records it;
  - open a pull request from a branch of this repository and mention the
    App in its body: the agent runs on the PR's head branch (\`balerix
    status\` shows the branch); submit a review with two inline comments:
    the status comment says "review from @you delivered" and the agent's
    next turn shows it read them;
  - close the issue: the agent is removed (\`balerix status gh-…\` no longer
    lists it), the status comment says "closed"; comment again: confused
    and "this session has ended";
  - wait past the three-minute idleTimeout on the PR session: "stopped
    after 3m idle"; mention the App again on the PR: the session resumes
    and the first prompt carries the "Earlier work" line;
  - edit .balerix.yaml on the default branch to set \`defaults: { sandbox:
    { extends: none } }\` and mention the App on a new issue: the refusal
    "defaults.sandbox: not allowed in a plugin-applied fleet file; the
    host's default applies" is posted with confused, and nothing starts;
    revert the edit;
  - restart the daemon: the sessions survive (a comment on the open issue
    still reaches its agent).
NOTES
```

`mise.toml`, after `verify-matrix`:

```toml
[tasks.verify-github]
description = "Spec M's manual check against a real GitHub App on a scratch repository (needs GITHUB_* settings; not part of any CI tier)"
run = "scripts/verify-github.sh"
```

- [ ] **Step 2: Register the plugin everywhere the others are listed**

Each edit mirrors the existing line for `matrix`:

- `mise.toml` `fmt`: add `"scripts/plugin.sh fmt github",`; `plugins`: add `"scripts/plugin.sh check github",`; `audit`: add `"cargo audit --file plugins/github/Cargo.lock",` and `"cargo deny --manifest-path plugins/github/Cargo.toml check advisories bans sources licenses",`; `release-prepare`'s `arg "<unit>" help="core, common, flow, web, matrix or github"`.
- `scripts/package-plugins.sh:15`: `names=(flow web matrix github)`.
- `.github/workflows/ci.yml:57`: `plugin: [common, flow, web, matrix, github]`; line 49's "four Rust caches" becomes "five".
- `.github/workflows/release-pr.yml`: line 12 `options: [core, common, flow, web, matrix, github]`; line 31's list gains `"github"`.
- `.github/actions/build-binary/action.yml:8` and `build-image/action.yml:9`: `(core, flow, web, matrix, github)`.
- `scripts/release/lib.sh`: `UNITS=(core common flow web matrix github)`, `PLUGIN_UNITS=(flow web matrix github)`, `IMAGE_UNITS=(core flow web matrix github)`, and `github` in the `require_unit` case on line 34.
- `scripts/release/test.sh`: `github` joins the loops at 98, 162, 164, 508 and 514; the six expectations at 416-434 become `'["core","flow","web","matrix","github"]'`; line 436 becomes `'["web","matrix","github"]'` (the plugin depends on common).
- `.vscode/settings.json`: add `"plugins/github/Cargo.toml"`.
- `docs/RELEASING.md`: the table row `| github | \`balerix-plugin-github-v<ver>\` | binaries, \`ghcr.io/balerix-ai/balerix-plugin-github\`, release package |`; line 141 "all four legs" → "all five legs"; line 149's first-release order gains ", then github (after common's)".

Run: `mise run release-test`
Expected: PASS (every scenario knows the new unit).

- [ ] **Step 3: The documentation**

- `AGENTS.md`: line 14 "all four" → "all five"; line 24 "all three" → "all four"; line 49 the unit list gains `github`; after the `verify-questions` bullet add:

```
- `verify-github` — Spec M's manual check against a real GitHub App on a
  scratch repository (`scripts/verify-github.sh`); needs `GITHUB_APP_ID`,
  `GITHUB_APP_KEY`, `GITHUB_WEBHOOK_SECRET`, `GITHUB_REPO` and a listener
  GitHub can reach. Not part of any CI tier.
```

- `ARCHITECTURE.md`, after the matrix paragraph:

```
- `balerix-plugin-github` — the fourth in-tree plugin: a mention of a
  GitHub App on an issue or pull request starts an agent whose session is
  that issue (Spec M). The fleet is the repository's own `.balerix.yaml`,
  applied through `manage` (Spec L) with the plugin's agents injected;
  turns and questions post as comments, permitted comments come back as
  prompts or answers (eyes when typed, +1 when Claude takes it, through
  common's `delivery`), a submitted review is one message, and close,
  merge or idle removes the agent. `webhook.rs` is the listener,
  `client.rs` the ten-endpoint `reqwest` client behind `GitHubPort`,
  `repo_config.rs`, `mention.rs`, `prompt.rs` and `status.rs` the pure
  parts, `actor.rs` the one task that owns the state.
```

- `README.md`: the `package-plugins` sentence lists `flow`, `web`, `matrix` and `github`; the Status paragraph gains one sentence naming the GitHub plugin (Spec M).
- `plugins/common/README.md`: line 7 → "The matrix and GitHub plugins are built on it."; lines 129-131 → "…the one message the web plugin and the GitHub plugin deliver."
- `docs/THREAT-MODEL.md`: line 100's "four committed `Cargo.lock` files" → "six" (the core workspace and five standalone plugin projects) and "over all four" → "over all six"; add the three Spec M §12.3 entries: under accepted risks the **GitHub G-5** bullet (§12.3's text, verbatim), under untrusted input a bullet "**GitHub webhooks and comment text.** …" (§12.3's "Untrusted input" paragraph), and in the table a row `| A GitHub webhook | HMAC-SHA256 over the raw body against the App's secret, constant-time; 1 MiB cap; delivery ids deduplicated; \`installation.id\` and \`repository.full_name\` from the verified payload only; issue, comment and review text are prompt text, never commands; the mention and the answer grammar are the only things parsed | \`plugins/github/src/webhook.rs\`, \`mention.rs\`, \`actor.rs::permitted\` |` and a row for the secrets (`privateKey`, `webhookSecret` via `secrets` 0600 files or literals; `Secret` newtypes; installation tokens memory-only, refreshed hourly; the plugin never sees Claude credentials or the gh token).

- Append to the Spec M file:

```
## 18. Recorded at implementation, part 3 (the plugin)

- Dependencies are path-only on the core crates (AGENTS.md); `base64`
  was added for the `contents` API, beyond §3's list.
- `GitHubPort` gained `issue(installation, repo, number)`: a mention in
  a later comment carries no title or body, so the prompt fetches them.
- The webhook's 413 is axum's body limit and is not counted
  (`too_large` was dropped from `webhooks_total`).
- `Tick` is every five seconds; the idle check, the two-second status
  coalescing, delivery expiry and row write-back all ride on it.
- The status comment continues across an idle stop and a resume: the row
  keeps `status_comment`.
- §6's `restarted: settings changed` line is not produced: the plugin
  sees a reconciler restart as a `SessionEnd`/`SessionStart` pair and does
  not know the reason; the two lines it does post say as much.
- §11's `github_requests_total{endpoint,status}` is not produced: the
  client counts nothing, `errors_total{kind}` covers failures.
- Health: `health.fail` on an `Auth` error or a dead listener,
  `health.ok()` on the next successful apply or comment.
- #64 closes with this part (§15.9) and #85 closed with part 2.
```

- [ ] **Step 4: The whole tier and the commit**

Run: `mise run check && mise run plugins && mise run package-plugins && mise run release-test`
Expected: PASS, and `target/plugins/github/` assembled.

```bash
git add mise.toml scripts .github .vscode docs AGENTS.md ARCHITECTURE.md README.md plugins/common/README.md
git commit -m "chore(github): register the plugin as a release unit, its manual check, and the docs (Spec M §15)"
```

Open the PR with the title from Global Constraints and a body that says "Closes #64" and points at `mise run verify-github` for the by-hand items of Spec M §15.2 to §15.6, which have to be run and their outcome pasted into the PR before merge.

---

## Done when

1. `mise run plugin github` passes; `mise run check`, `mise run plugins`, `mise run package-plugins` and `mise run release-test` pass with the new unit.
2. Spec M §15.1 to §15.7 hold, the by-hand ones through `mise run verify-github` on a scratch repository, and §15.8 and §15.9 (the two-step reactions, the restricted refusal on the issue).
3. `target/plugins/github/bin/balerix-plugin-github` prints `github: …` and exits 1 without the daemon's environment.
4. The first `release-prepare github` after merge proposes 0.1.0 with `- Initial release.`
