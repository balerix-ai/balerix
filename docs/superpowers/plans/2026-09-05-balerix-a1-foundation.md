# Balerix Spec A / Phase 1 — Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the Cargo workspace and ship the pure half of balerix — wire types, validated domain names, and the YAML → resolved `FleetSpec` pipeline — exposed through `balerix config resolve`.

**Architecture:** Three library crates and one binary. `balerix-api` holds serde wire types. `balerix-core` holds validated newtypes and the domain `Fleet`, converting from the wire types with `TryFrom` ("parse, don't validate"). `balerix-config` parses the three-level YAML file, deep-merges the settings layers as `serde_json::Value`, deserializes each agent's merged layer into `AgentSettings`, validates, and produces a fully-resolved `FleetSpec`. The `balerix` binary wraps that in `config resolve`. Nothing in this phase spawns a process or touches the network.

**Tech Stack:** Rust 1.98.1 (edition 2024), serde / serde_json / serde_norway (maintained `serde_yaml` fork), thiserror, anyhow, clap 4, insta (golden), proptest (property), assert_cmd (CLI), cargo-nextest. Tools pinned via mise.

**Spec:** `docs/superpowers/specs/2026-09-05-balerix-architecture-design.md` — this plan implements §3 (crate layout, partially), §5 (configuration model), and the Foundation bullets of §10 and §11. Read §5 before starting any config task.

## Global Constraints

Copied from the spec; every task's requirements include these.

- Rust **1.98.1**, `edition = "2024"`, `rust-version = "1.98"`, installed via mise (`rust = "1.98.1"`). Every tool in `mise.toml` is an exact version.
- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` must pass at every commit. `unsafe_code = "forbid"` workspace-wide.
- Library crates return `thiserror` errors; only the `balerix` binary uses `anyhow`. No panics in library code (no `unwrap`/`expect` outside tests).
- Names (`fleet`, `crew`, `agent`) match `[a-z0-9]([a-z0-9-]*[a-z0-9])?`, length 1–63.
- Settings block keys are exactly: `claude`, `sandbox`, `tools`, `env`, `runner`, `flow`. Merge rules: maps deep-merge, scalars overlay-wins, lists replace, explicit `null` deletes.
- `tools` values must be exact versions (§5, D9). `claude.settings.hooks` is balerix-owned and rejected in user config.
- Secrets (`CredentialBundle`) never appear in `Debug` output, logs, or `config resolve` output.
- Dependency direction (§3): `api` is a leaf; `core` → `api`; `config` → `core`, `api`; bin → everything. No adapter crate depends on another adapter crate.
- Cargo dependencies use caret versions with the exact versions listed in Task 1; `Cargo.lock` is committed.
- Run ad-hoc cargo commands as `mise x -- cargo …` so the pinned toolchain is used regardless of shell activation. `mise run <task>` already does this.

## File structure

```
Cargo.toml                          workspace: members, shared deps, lints
mise.toml                           pinned tools + tasks (fmt, lint, test, check, precommit, audit)
clippy.toml                         unwrap/expect allowed in tests only
deny.toml                           cargo-deny policy
.gitignore
.githooks/pre-commit                → mise run precommit
README.md  AGENTS.md  CLAUDE.md  ARCHITECTURE.md
docs/THREAT-MODEL.md                Tier-1 threat model (from spec §10)
crates/balerix-api/
  src/lib.rs                        re-exports + API_VERSION
  src/settings.rs                   AgentSettings, ClaudeSettings, RunnerSettings
  src/fleet.rs                      FleetSpec, CrewSpec, GitSettings, GitAuth
  src/credentials.rs                CredentialBundle (redacting Debug)
  src/request.rs                    FleetRequest
crates/balerix-core/
  src/lib.rs                        re-exports
  src/name.rs                       FleetName, CrewName, AgentName, AgentId, NameError
  src/repo.rs                       RepoRef (GitHub slug or URL)
  src/fleet.rs                      Fleet, Crew, FleetError, TryFrom<api::FleetSpec>
crates/balerix-config/
  src/lib.rs                        re-exports
  src/error.rs                      ConfigError
  src/merge.rs                      merge, merge_layers, strip_nulls
  src/file.rs                       FleetFile, CrewFile, parse
  src/validate.rs                   validate_agent, is_exact_version
  src/resolve.rs                    ResolveOptions, resolve
  src/host.rs                       HostPaths, HostDefaults, load
  tests/fixtures/payments.yaml      spec §5 example (exact versions)
  tests/fixtures/overrides.yaml     null-delete + list-replace exercise
  tests/resolve_golden.rs           insta snapshots of resolved specs
crates/balerix/
  src/main.rs                       entry; anyhow error → exit 1
  src/cli.rs                        clap definitions
  src/commands/mod.rs
  src/commands/config.rs            `config resolve`
  tests/cli_config_resolve.rs       assert_cmd tests
```

---

### Task 1: Workspace scaffold, pinned toolchain, tasks

**Files:**
- Create: `mise.toml`, `Cargo.toml`, `clippy.toml`, `deny.toml`, `.gitignore`, `.githooks/pre-commit`
- Create: `crates/balerix-api/Cargo.toml`, `crates/balerix-api/src/lib.rs`
- Create: `CLAUDE.md` (one line), `AGENTS.md`, `README.md`, `ARCHITECTURE.md` (skeletons; filled in Task 12)

**Interfaces:**
- Produces: `balerix_api::API_VERSION: &str = "balerix/v1"`; workspace dependency table every later task's `Cargo.toml` refers to with `{ workspace = true }`; mise tasks `fmt`, `lint`, `test`, `check`, `precommit`, `audit`.

- [ ] **Step 1: Write `mise.toml`**

```toml
# Every entry is an exact version (developer-environment: an unpinned entry is a reproducibility bug).
# git is a system prerequisite (not in the mise registry); the devcontainer ships 2.47.3.
[tools]
rust = "1.98.1"
"cargo:cargo-nextest" = "0.9.143"
cargo-insta = "1.48.0"
"cargo:cargo-audit" = "0.22.2"
"aqua:EmbarkStudios/cargo-deny" = "0.20.2"
"github:sourcefrog/cargo-mutants" = "27.1.0"
gitleaks = "8.30.1"
tmux = "3.7c"
gh = "2.100.0"
"github:always-further/nono" = "0.75.0"
claude = "2.1.261"

[tasks.fmt]
description = "Format all crates"
run = "cargo fmt --all"

[tasks.lint]
description = "Static tier: rustfmt check + clippy with warnings denied"
run = [
  "cargo fmt --all --check",
  "cargo clippy --workspace --all-targets -- -D warnings",
]

[tasks.test]
description = "Unit + integration tests"
run = "cargo nextest run --workspace"

[tasks.check]
description = "Everything the PR tier runs"
depends = ["lint", "test"]

[tasks.precommit]
description = "Pre-commit tier: staged secret scan, then the PR gate"
run = [
  "gitleaks git --pre-commit --staged --redact",
  "mise run check",
]

[tasks.audit]
description = "Nightly tier: dependency CVEs and policy"
run = [
  "cargo audit",
  "cargo deny check advisories bans sources licenses",
]
```

- [ ] **Step 2: Install the toolchain and verify the pins**

Run: `mise trust && mise install`
Expected: installs complete without error (the `cargo:` backends compile from source; several minutes is normal).

Run: `mise x -- cargo --version && mise x -- cargo nextest --version && mise x -- cargo insta --version && mise x -- cargo fmt --version && mise x -- cargo clippy --version`
Expected: `cargo 1.98.1 …`, `cargo-nextest 0.9.143`, `cargo-insta 1.48.0`, and rustfmt/clippy versions. If `cargo fmt` or `cargo clippy` is missing: `mise x -- rustup component add rustfmt clippy`, re-check.

- [ ] **Step 3: Write the workspace `Cargo.toml`**

```toml
[workspace]
resolver = "3"
members = ["crates/*"]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.98"
license = "Apache-2.0"
repository = "https://github.com/balerix-ai/balerix"

[workspace.dependencies]
balerix-api = { path = "crates/balerix-api" }
balerix-core = { path = "crates/balerix-core" }
balerix-config = { path = "crates/balerix-config" }

serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
serde_norway = "0.9.42"
thiserror = "2.0.20"
anyhow = "1.0.104"
clap = { version = "4.6.6", features = ["derive"] }

# dev
insta = { version = "1.48.0", features = ["yaml", "json"] }
proptest = "1.11.0"
tempfile = "3.27.0"
pretty_assertions = "1.4.1"
assert_cmd = "2.2.2"
predicates = "3.1.4"

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
unwrap_used = "warn"
expect_used = "warn"
```

- [ ] **Step 4: Create `balerix-api` with one constant and one test**

`crates/balerix-api/Cargo.toml`:
```toml
[package]
name = "balerix-api"
description = "Wire types shared by the balerix CLI and daemon"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }

[lints]
workspace = true
```

`crates/balerix-api/src/lib.rs`:
```rust
//! Wire types shared by the balerix CLI and daemon (spec §3).
//!
//! This crate is a leaf: serde DTOs only, no logic beyond defaults and
//! secret-redacting `Debug` impls.

/// The `apiVersion` every fleet file and request declares.
pub const API_VERSION: &str = "balerix/v1";

#[cfg(test)]
mod tests {
    use super::API_VERSION;

    #[test]
    fn api_version_is_v1() {
        assert_eq!(API_VERSION, "balerix/v1");
    }
}
```

- [ ] **Step 5: Write `clippy.toml`, `deny.toml`, `.gitignore`, and the pre-commit hook**

`clippy.toml` — the workspace warns on `unwrap`/`expect` so library code stays panic-free; tests are exempt:
```toml
allow-unwrap-in-tests = true
allow-expect-in-tests = true
```

`deny.toml`:
```toml
[advisories]
ignore = []

[licenses]
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

`.gitignore`:
```
/target
*.snap.new
.mise.local.toml
```

`.githooks/pre-commit`:
```sh
#!/usr/bin/env sh
exec mise run precommit
```
Run: `chmod +x .githooks/pre-commit && git config core.hooksPath .githooks`

- [ ] **Step 6: Write the doc skeletons**

`CLAUDE.md`:
```markdown
See AGENTS.md.
```

`AGENTS.md`:
```markdown
# Balerix — agent instructions

Read `ARCHITECTURE.md` for where code lives and why. The design is in
`docs/superpowers/specs/2026-09-05-balerix-architecture-design.md`.

## Tasks (run with `mise run <task>`)
- `check` — everything the PR tier runs (lint + test). Run before every commit.
- `lint`, `test`, `fmt`, `precommit`, `audit` — see `mise.toml` for what each does.

## Conventions
- Ports (`AgentRunner`, `FleetStore`, `EventHandler`) live in `balerix-core`; adapters implement them and never depend on each other.
- Library crates return `thiserror` errors; only the `balerix` binary uses `anyhow`.
- Every tool version — in `mise.toml` and in fleet config `tools:` — is exact.
- Types that hold secrets implement `Debug` by hand and print `<redacted>`.

## Gotchas
- Run cargo through mise: `mise x -- cargo …` (or a `mise run` task).
- `git config core.hooksPath .githooks` once after cloning to get the pre-commit tier.
- insta snapshots: review the `.snap.new` file, then `mise x -- cargo insta accept`. Never blind-accept.
```

`README.md`:
```markdown
# balerix

A control plane and orchestrator for fleets of coding agents (Claude Code first).

## Quickstart
1. `mise trust && mise install`
2. `git config core.hooksPath .githooks`
3. `mise run check`

See `ARCHITECTURE.md` for how the pieces fit and `AGENTS.md` for conventions.
```

`ARCHITECTURE.md`:
```markdown
# Architecture

(Filled in at the end of Phase 1 — see Task 12.)
```

- [ ] **Step 7: Verify the whole PR tier passes on the scaffold**

Run: `mise run check`
Expected: fmt check passes, clippy passes with zero warnings, nextest reports `1 test run: 1 passed`.

Run: `mise run audit`
Expected: `cargo audit` finds 0 vulnerabilities; `cargo deny` passes. If deny fails only on a license name, add that SPDX id to `deny.toml` `allow` and re-run.

- [ ] **Step 8: Commit**

```bash
git add mise.toml Cargo.toml Cargo.lock clippy.toml deny.toml .gitignore .githooks crates/balerix-api CLAUDE.md AGENTS.md README.md ARCHITECTURE.md
git commit -m "Scaffold cargo workspace with pinned mise toolchain and tasks"
```

---

### Task 2: `balerix-api` — the settings block

**Files:**
- Create: `crates/balerix-api/src/settings.rs`
- Modify: `crates/balerix-api/src/lib.rs` (add `pub mod settings; pub use settings::*;`)

**Interfaces:**
- Produces:
  ```rust
  pub struct AgentSettings { pub claude: ClaudeSettings, pub sandbox: Value, pub tools: BTreeMap<String,String>, pub env: BTreeMap<String,String>, pub runner: RunnerSettings, pub flow: Value }
  pub struct ClaudeSettings { pub settings: Value, pub args: Vec<String>, pub resume: bool, pub binary: String }
  pub enum RunnerSettings { Tmux }          // serde: { "type": "tmux" }
  ```
  All three implement `Debug, Clone, PartialEq, Serialize, Deserialize, Default`. Unknown fields are rejected.

- [ ] **Step 1: Write the failing tests**

Append to `crates/balerix-api/src/settings.rs` (create the file with only this test module for now):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_settings_are_tmux_with_claude_binary_and_empty_blocks() {
        let s = AgentSettings::default();
        assert_eq!(s.runner, RunnerSettings::Tmux);
        assert_eq!(s.claude.binary, "claude");
        assert!(!s.claude.resume);
        assert!(s.claude.args.is_empty());
        assert_eq!(s.claude.settings, json!({}));
        assert_eq!(s.sandbox, json!({}));
        assert_eq!(s.flow, json!({}));
        assert!(s.tools.is_empty());
        assert!(s.env.is_empty());
    }

    #[test]
    fn deserializes_a_full_block() {
        let v = json!({
            "claude": { "settings": { "model": "opus" }, "args": ["--verbose"], "resume": true, "binary": "/opt/claude" },
            "sandbox": { "network": { "mode": "allow" } },
            "tools": { "node": "22.11.0" },
            "env": { "RUST_LOG": "info" },
            "runner": { "type": "tmux" },
            "flow": {}
        });
        let s: AgentSettings = serde_json::from_value(v).unwrap();
        assert_eq!(s.claude.settings, json!({ "model": "opus" }));
        assert_eq!(s.claude.args, vec!["--verbose"]);
        assert!(s.claude.resume);
        assert_eq!(s.claude.binary, "/opt/claude");
        assert_eq!(s.tools["node"], "22.11.0");
        assert_eq!(s.env["RUST_LOG"], "info");
        assert_eq!(s.runner, RunnerSettings::Tmux);
    }

    #[test]
    fn missing_blocks_take_defaults() {
        let s: AgentSettings = serde_json::from_value(json!({ "tools": { "python": "3.12.8" } })).unwrap();
        assert_eq!(s.claude, ClaudeSettings::default());
        assert_eq!(s.runner, RunnerSettings::Tmux);
        assert_eq!(s.tools["python"], "3.12.8");
    }

    #[test]
    fn rejects_unknown_top_level_and_claude_fields() {
        assert!(serde_json::from_value::<AgentSettings>(json!({ "claud": {} })).is_err());
        assert!(serde_json::from_value::<AgentSettings>(json!({ "claude": { "model": "opus" } })).is_err());
    }

    #[test]
    fn rejects_unknown_runner_type() {
        assert!(serde_json::from_value::<AgentSettings>(json!({ "runner": { "type": "docker" } })).is_err());
    }

    #[test]
    fn round_trips_through_json() {
        let s = AgentSettings {
            tools: BTreeMap::from([("node".to_string(), "22.11.0".to_string())]),
            ..AgentSettings::default()
        };
        let back: AgentSettings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
```

Add to `lib.rs`: `pub mod settings;` and `pub use settings::{AgentSettings, ClaudeSettings, RunnerSettings};`

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-api`
Expected: compile error — `AgentSettings` not found.

- [ ] **Step 3: Implement the types**

Prepend to `crates/balerix-api/src/settings.rs` (above the test module):

```rust
//! The per-agent settings block (spec §5). Appears at fleet, crew and agent
//! level in the YAML file; after resolution every agent carries one complete
//! copy.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Fully-resolved settings for one agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSettings {
    #[serde(default)]
    pub claude: ClaudeSettings,
    /// Mirrors the nono profile schema; passthrough map.
    #[serde(default = "empty_object")]
    pub sandbox: Value,
    /// Tool → exact version, rendered into the agent's `mise.toml`.
    #[serde(default)]
    pub tools: BTreeMap<String, String>,
    /// Extra environment variables appended after balerix's own.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub runner: RunnerSettings,
    /// Reserved for the state-machine spec; passthrough map.
    #[serde(default = "empty_object")]
    pub flow: Value,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            claude: ClaudeSettings::default(),
            sandbox: empty_object(),
            tools: BTreeMap::new(),
            env: BTreeMap::new(),
            runner: RunnerSettings::default(),
            flow: empty_object(),
        }
    }
}

/// How Claude Code itself is configured and launched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeSettings {
    /// Merged verbatim into the agent's `settings.json`; passthrough map.
    #[serde(default = "empty_object")]
    pub settings: Value,
    /// Extra CLI arguments passed to the claude binary.
    #[serde(default)]
    pub args: Vec<String>,
    /// Start with `--continue` when a preserved session exists.
    #[serde(default)]
    pub resume: bool,
    /// Binary to launch; overridable for tests and alternative builds.
    #[serde(default = "default_binary")]
    pub binary: String,
}

impl Default for ClaudeSettings {
    fn default() -> Self {
        Self {
            settings: empty_object(),
            args: Vec::new(),
            resume: false,
            binary: default_binary(),
        }
    }
}

/// Which runner materializes the agent. Only tmux exists today (spec §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RunnerSettings {
    #[default]
    Tmux,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn default_binary() -> String {
    "claude".to_string()
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-api`
Expected: 7 tests pass (6 new + `api_version_is_v1`).

- [ ] **Step 5: Lint and commit**

Run: `mise run lint`
Expected: clean.

```bash
git add crates/balerix-api
git commit -m "Add AgentSettings wire type with defaults and strict fields"
```

---

### Task 3: `balerix-api` — FleetSpec, CredentialBundle, FleetRequest

**Files:**
- Create: `crates/balerix-api/src/fleet.rs`, `crates/balerix-api/src/credentials.rs`, `crates/balerix-api/src/request.rs`
- Modify: `crates/balerix-api/src/lib.rs`

**Interfaces:**
- Consumes: `AgentSettings` (Task 2).
- Produces:
  ```rust
  pub struct FleetSpec { pub name: String, pub crews: BTreeMap<String, CrewSpec> }
  pub struct CrewSpec { pub repo: String, pub git_ref: String /* serde "ref" */, pub git: GitSettings, pub agents: BTreeMap<String, AgentSettings> }
  pub struct GitSettings { pub push: bool /* default true */, pub auth: GitAuth /* default Gh */ }
  pub enum GitAuth { Gh, None }             // serde lowercase
  pub struct CredentialBundle { pub claude_credentials: Option<Value>, pub claude_account: Option<Value>, pub gh_token: Option<String> }  // Debug redacts
  pub struct FleetRequest { pub spec: FleetSpec, pub credentials: CredentialBundle }
  ```

- [ ] **Step 1: Write the failing tests**

`crates/balerix-api/src/fleet.rs` (tests only for now):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn git_settings_default_to_push_with_gh_auth() {
        let g = GitSettings::default();
        assert!(g.push);
        assert_eq!(g.auth, GitAuth::Gh);
    }

    #[test]
    fn crew_spec_uses_ref_as_the_wire_name() {
        let c: CrewSpec = serde_json::from_value(json!({
            "repo": "acme/payments-api", "ref": "main", "git": { "push": false, "auth": "none" },
            "agents": { "alice": {} }
        }))
        .unwrap();
        assert_eq!(c.git_ref, "main");
        assert!(!c.git.push);
        assert_eq!(c.git.auth, GitAuth::None);
        assert!(c.agents.contains_key("alice"));
        let back = serde_json::to_value(&c).unwrap();
        assert_eq!(back["ref"], "main");
        assert_eq!(back["git"]["auth"], "none");
    }

    #[test]
    fn fleet_spec_round_trips() {
        let spec = FleetSpec {
            name: "payments".into(),
            crews: BTreeMap::from([(
                "backend".to_string(),
                CrewSpec {
                    repo: "acme/payments-api".into(),
                    git_ref: "main".into(),
                    git: GitSettings::default(),
                    agents: BTreeMap::from([("alice".to_string(), AgentSettings::default())]),
                },
            )]),
        };
        let back: FleetSpec = serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        assert_eq!(back, spec);
    }
}
```

`crates/balerix-api/src/credentials.rs` (tests only):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn debug_never_prints_secret_material() {
        let b = CredentialBundle {
            claude_credentials: Some(json!({ "claudeAiOauth": { "accessToken": "sk-ant-SECRET" } })),
            claude_account: Some(json!({ "oauthAccount": { "emailAddress": "a@b.c" } })),
            gh_token: Some("gho_SECRET".into()),
        };
        let dbg = format!("{b:?}");
        assert!(!dbg.contains("SECRET"), "debug leaked a secret: {dbg}");
        assert!(!dbg.contains("a@b.c"));
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn debug_shows_which_parts_are_present() {
        let b = CredentialBundle { claude_credentials: None, claude_account: None, gh_token: Some("x".into()) };
        let dbg = format!("{b:?}");
        assert!(dbg.contains("claude_credentials: None"));
        assert!(dbg.contains("gh_token: Some(<redacted>)"));
    }

    #[test]
    fn serializes_fully_for_the_wire() {
        let b = CredentialBundle { claude_credentials: None, claude_account: None, gh_token: Some("gho_x".into()) };
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v["gh_token"], "gho_x");
    }
}
```

`crates/balerix-api/src/request.rs` (tests only):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn request_round_trips() {
        let r = FleetRequest {
            spec: FleetSpec { name: "f".into(), crews: BTreeMap::new() },
            credentials: CredentialBundle::default(),
        };
        let back: FleetRequest = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back.spec, r.spec);
        assert_eq!(back.credentials.gh_token, None);
    }
}
```

`lib.rs` additions:
```rust
pub mod credentials;
pub mod fleet;
pub mod request;
pub mod settings;

pub use credentials::CredentialBundle;
pub use fleet::{CrewSpec, FleetSpec, GitAuth, GitSettings};
pub use request::FleetRequest;
pub use settings::{AgentSettings, ClaudeSettings, RunnerSettings};
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-api`
Expected: compile errors for the missing types.

- [ ] **Step 3: Implement**

Top of `crates/balerix-api/src/fleet.rs`:
```rust
//! The resolved fleet specification — the wire format for `up`/`update`
//! and what the daemon stores (spec §5, §7).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::settings::AgentSettings;

/// A fully-resolved fleet: every agent carries a complete settings block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetSpec {
    pub name: String,
    #[serde(default)]
    pub crews: BTreeMap<String, CrewSpec>,
}

/// One crew: a shared repository plus its agents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrewSpec {
    /// GitHub `owner/name` shorthand or a full clone URL.
    pub repo: String,
    /// Base ref for per-agent branches.
    #[serde(rename = "ref")]
    pub git_ref: String,
    #[serde(default)]
    pub git: GitSettings,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentSettings>,
}

/// Git permissions for a crew.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitSettings {
    #[serde(default = "default_true")]
    pub push: bool,
    #[serde(default)]
    pub auth: GitAuth,
}

impl Default for GitSettings {
    fn default() -> Self {
        Self { push: true, auth: GitAuth::default() }
    }
}

/// Where the crew's git credentials come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitAuth {
    /// Token from the host's `gh` config, sent in the credential bundle.
    #[default]
    Gh,
    /// No credentials; public repos only.
    None,
}

fn default_true() -> bool {
    true
}
```

Top of `crates/balerix-api/src/credentials.rs`:
```rust
//! Host credentials the client sends alongside a spec (spec §5, §7). The
//! daemon encrypts them at rest; this type never prints their contents.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Credentials discovered on the client host.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CredentialBundle {
    /// Contents of `~/.claude/.credentials.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_credentials: Option<Value>,
    /// Account fields lifted from `~/.claude.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_account: Option<Value>,
    /// `gh` OAuth token for the crew repos.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gh_token: Option<String>,
}

impl fmt::Debug for CredentialBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialBundle")
            .field("claude_credentials", &Redacted(self.claude_credentials.is_some()))
            .field("claude_account", &Redacted(self.claude_account.is_some()))
            .field("gh_token", &Redacted(self.gh_token.is_some()))
            .finish()
    }
}

/// Prints `Some(<redacted>)` or `None` without touching the value.
struct Redacted(bool);

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 { f.write_str("Some(<redacted>)") } else { f.write_str("None") }
    }
}
```

Top of `crates/balerix-api/src/request.rs`:
```rust
//! Request bodies for the fleet endpoints (spec §7).

use serde::{Deserialize, Serialize};

use crate::{CredentialBundle, FleetSpec};

/// Body of `POST /v1/fleets` (up) and `PUT /v1/fleets/{name}` (update).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetRequest {
    pub spec: FleetSpec,
    #[serde(default)]
    pub credentials: CredentialBundle,
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-api`
Expected: 14 tests pass.

- [ ] **Step 5: Lint and commit**

Run: `mise run lint`

```bash
git add crates/balerix-api
git commit -m "Add FleetSpec, CredentialBundle and FleetRequest wire types"
```

---

### Task 4: `balerix-core` — validated names

**Files:**
- Create: `crates/balerix-core/Cargo.toml`, `crates/balerix-core/src/lib.rs`, `crates/balerix-core/src/name.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct FleetName(String); pub struct CrewName(String); pub struct AgentName(String);
  // each: FromStr, TryFrom<String>, TryFrom<&str>, Display, as_str(), Ord, Hash, Serialize, Deserialize (validating)
  pub struct AgentId { pub fleet: FleetName, pub crew: CrewName, pub agent: AgentName }  // Display "fleet/crew/agent", FromStr
  pub struct NameError { pub kind: &'static str, pub value: String, pub reason: &'static str }
  pub fn validate_name(s: &str) -> Result<(), &'static str>
  ```

- [ ] **Step 1: Create the crate and write the failing tests**

`crates/balerix-core/Cargo.toml`:
```toml
[package]
name = "balerix-core"
description = "Domain model and ports for balerix"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
balerix-api = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }

[dev-dependencies]
proptest = { workspace = true }

[lints]
workspace = true
```

`crates/balerix-core/src/lib.rs`:
```rust
//! Domain model and ports (spec §3). No I/O lives here.

pub mod name;

pub use name::{AgentId, AgentName, CrewName, FleetName, NameError};
```

`crates/balerix-core/src/name.rs` (tests only for now):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn accepts_dns_label_like_names() {
        let longest = "x".repeat(63);
        for ok in ["a", "payments", "backend-1", "0abc", "a-b-c", longest.as_str()] {
            assert!(FleetName::try_from(ok).is_ok(), "{ok:?} should be valid");
        }
    }

    #[test]
    fn rejects_bad_names_with_a_reason() {
        let too_long = "x".repeat(64);
        let cases = [
            ("", "empty"),
            (too_long.as_str(), "longer than 63 characters"),
            ("Payments", "contains characters other than a-z, 0-9 and '-'"),
            ("back_end", "contains characters other than a-z, 0-9 and '-'"),
            ("-lead", "starts or ends with '-'"),
            ("trail-", "starts or ends with '-'"),
            ("a/b", "contains characters other than a-z, 0-9 and '-'"),
        ];
        for (bad, reason) in cases {
            let err = CrewName::try_from(bad).unwrap_err();
            assert_eq!(err.kind, "crew");
            assert_eq!(err.value, bad);
            assert_eq!(err.reason, reason, "for {bad:?}");
        }
    }

    #[test]
    fn error_message_names_the_kind_value_and_reason() {
        let err = AgentName::try_from("Bob").unwrap_err();
        assert_eq!(err.to_string(), "invalid agent name \"Bob\": contains characters other than a-z, 0-9 and '-'");
    }

    #[test]
    fn serde_deserialization_validates() {
        assert!(serde_json::from_str::<FleetName>("\"ok-name\"").is_ok());
        assert!(serde_json::from_str::<FleetName>("\"Not Ok\"").is_err());
        assert_eq!(serde_json::to_string(&FleetName::try_from("f").unwrap()).unwrap(), "\"f\"");
    }

    #[test]
    fn agent_id_displays_and_parses_as_three_segments() {
        let id: AgentId = "payments/backend/alice".parse().unwrap();
        assert_eq!(id.fleet.as_str(), "payments");
        assert_eq!(id.crew.as_str(), "backend");
        assert_eq!(id.agent.as_str(), "alice");
        assert_eq!(id.to_string(), "payments/backend/alice");
        assert!("payments/backend".parse::<AgentId>().is_err());
        assert!("a/b/c/d".parse::<AgentId>().is_err());
        assert!("a/B/c".parse::<AgentId>().is_err());
    }

    proptest! {
        #[test]
        fn every_generated_valid_name_parses(s in "[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?") {
            prop_assert!(FleetName::try_from(s.as_str()).is_ok());
        }

        #[test]
        fn parse_then_display_is_identity(s in "[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?") {
            let n = AgentName::try_from(s.as_str()).unwrap();
            prop_assert_eq!(n.to_string(), s);
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-core`
Expected: compile errors — the types do not exist.

- [ ] **Step 3: Implement**

Prepend to `crates/balerix-core/src/name.rs`:
```rust
//! Validated identifiers (spec §5: names match `[a-z0-9-]+`, DNS-label
//! style so they survive as tmux session names, branch names and, later,
//! Kubernetes labels).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Why a string is not a valid name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind} name {value:?}: {reason}")]
pub struct NameError {
    pub kind: &'static str,
    pub value: String,
    pub reason: &'static str,
}

/// Checks the shared naming rule. Returns the human-readable reason on failure.
pub fn validate_name(s: &str) -> Result<(), &'static str> {
    if s.is_empty() {
        return Err("empty");
    }
    if s.len() > 63 {
        return Err("longer than 63 characters");
    }
    if !s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Err("contains characters other than a-z, 0-9 and '-'");
    }
    if s.starts_with('-') || s.ends_with('-') {
        return Err("starts or ends with '-'");
    }
    Ok(())
}

macro_rules! name_type {
    ($(#[$doc:meta])* $t:ident, $kind:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $t(String);

        impl $t {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $t {
            type Error = NameError;
            fn try_from(value: String) -> Result<Self, NameError> {
                validate_name(&value)
                    .map(|()| Self(value.clone()))
                    .map_err(|reason| NameError { kind: $kind, value, reason })
            }
        }

        impl TryFrom<&str> for $t {
            type Error = NameError;
            fn try_from(value: &str) -> Result<Self, NameError> {
                Self::try_from(value.to_string())
            }
        }

        impl FromStr for $t {
            type Err = NameError;
            fn from_str(s: &str) -> Result<Self, NameError> {
                Self::try_from(s)
            }
        }

        impl From<$t> for String {
            fn from(n: $t) -> String {
                n.0
            }
        }

        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

name_type!(
    /// Name of a fleet.
    FleetName, "fleet"
);
name_type!(
    /// Name of a crew within a fleet.
    CrewName, "crew"
);
name_type!(
    /// Name of an agent within a crew.
    AgentName, "agent"
);

/// Fully-qualified agent identity, written `fleet/crew/agent`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AgentId {
    pub fleet: FleetName,
    pub crew: CrewName,
    pub agent: AgentName,
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.fleet, self.crew, self.agent)
    }
}

impl FromStr for AgentId {
    type Err = NameError;

    fn from_str(s: &str) -> Result<Self, NameError> {
        let mut parts = s.splitn(4, '/');
        let (Some(f), Some(c), Some(a), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return Err(NameError { kind: "agent id", value: s.to_string(), reason: "expected exactly fleet/crew/agent" });
        };
        Ok(Self { fleet: f.parse()?, crew: c.parse()?, agent: a.parse()? })
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-core`
Expected: 7 tests pass (5 unit + 2 proptest).

- [ ] **Step 5: Lint and commit**

Run: `mise run lint`

```bash
git add crates/balerix-core Cargo.lock
git commit -m "Add balerix-core with validated fleet, crew and agent names"
```

---

### Task 5: `balerix-core` — `RepoRef` and the domain `Fleet` with `TryFrom<FleetSpec>`

**Files:**
- Create: `crates/balerix-core/src/repo.rs`, `crates/balerix-core/src/fleet.rs`
- Modify: `crates/balerix-core/src/lib.rs`

**Interfaces:**
- Consumes: `FleetName`, `CrewName`, `AgentName`, `NameError` (Task 4); `balerix_api::{FleetSpec, CrewSpec, GitSettings, AgentSettings}` (Tasks 2–3).
- Produces:
  ```rust
  pub enum RepoRef { GitHub { owner: String, name: String }, Url(String) }
  impl RepoRef { pub fn parse(s: &str) -> Result<RepoRef, RepoError>; pub fn clone_url(&self) -> String; pub fn github_slug(&self) -> Option<String> }
  pub struct RepoError { pub value: String, pub reason: &'static str }
  pub struct Fleet { pub name: FleetName, pub crews: BTreeMap<CrewName, Crew> }
  pub struct Crew { pub repo: RepoRef, pub git_ref: String, pub git: GitSettings, pub agents: BTreeMap<AgentName, AgentSettings> }
  pub enum FleetError { InvalidName { path: String, source: NameError }, InvalidRepo { path: String, source: RepoError }, EmptyRef { path: String } }
  impl TryFrom<balerix_api::FleetSpec> for Fleet
  impl From<Fleet> for balerix_api::FleetSpec
  ```
  `FleetError::to_string()` always starts with the config path, e.g. `crews.backend.agents.Bob: invalid agent name "Bob": …`.

- [ ] **Step 1: Write the failing tests**

`crates/balerix-core/src/repo.rs` (tests only):
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_shorthand_and_urls_to_the_same_ref() {
        let expected = RepoRef::GitHub { owner: "acme".into(), name: "payments-api".into() };
        for s in [
            "acme/payments-api",
            "https://github.com/acme/payments-api",
            "https://github.com/acme/payments-api.git",
            "git@github.com:acme/payments-api.git",
        ] {
            assert_eq!(RepoRef::parse(s).unwrap(), expected, "for {s}");
        }
    }

    #[test]
    fn github_ref_has_https_clone_url_and_slug() {
        let r = RepoRef::parse("acme/payments-api").unwrap();
        assert_eq!(r.clone_url(), "https://github.com/acme/payments-api.git");
        assert_eq!(r.github_slug().as_deref(), Some("acme/payments-api"));
    }

    #[test]
    fn non_github_urls_are_kept_verbatim() {
        let r = RepoRef::parse("https://gitlab.com/acme/x.git").unwrap();
        assert_eq!(r, RepoRef::Url("https://gitlab.com/acme/x.git".into()));
        assert_eq!(r.clone_url(), "https://gitlab.com/acme/x.git");
        assert_eq!(r.github_slug(), None);
    }

    #[test]
    fn rejects_malformed_values() {
        for (bad, reason) in [
            ("", "empty"),
            ("acme", "expected owner/name or a clone URL"),
            ("acme/", "expected owner/name or a clone URL"),
            ("/name", "expected owner/name or a clone URL"),
            ("a/b/c", "expected owner/name or a clone URL"),
            ("acme/pay ments", "expected owner/name or a clone URL"),
        ] {
            let err = RepoRef::parse(bad).unwrap_err();
            assert_eq!(err.value, bad);
            assert_eq!(err.reason, reason, "for {bad:?}");
        }
    }
}
```

`crates/balerix-core/src/fleet.rs` (tests only):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{AgentSettings, CrewSpec, FleetSpec, GitSettings};
    use std::collections::BTreeMap;

    fn spec(fleet: &str, crew: &str, repo: &str, git_ref: &str, agents: &[&str]) -> FleetSpec {
        FleetSpec {
            name: fleet.into(),
            crews: BTreeMap::from([(
                crew.to_string(),
                CrewSpec {
                    repo: repo.into(),
                    git_ref: git_ref.into(),
                    git: GitSettings::default(),
                    agents: agents.iter().map(|a| (a.to_string(), AgentSettings::default())).collect(),
                },
            )]),
        }
    }

    #[test]
    fn converts_a_valid_spec() {
        let f = Fleet::try_from(spec("payments", "backend", "acme/api", "main", &["alice", "bob"])).unwrap();
        assert_eq!(f.name.as_str(), "payments");
        let crew = &f.crews[&CrewName::try_from("backend").unwrap()];
        assert_eq!(crew.repo, RepoRef::GitHub { owner: "acme".into(), name: "api".into() });
        assert_eq!(crew.git_ref, "main");
        assert_eq!(crew.agents.len(), 2);
    }

    #[test]
    fn invalid_fleet_name_reports_path_name() {
        let err = Fleet::try_from(spec("Payments", "backend", "acme/api", "main", &[])).unwrap_err();
        assert_eq!(err.to_string(), "name: invalid fleet name \"Payments\": contains characters other than a-z, 0-9 and '-'");
    }

    #[test]
    fn invalid_crew_and_agent_names_report_their_paths() {
        let err = Fleet::try_from(spec("payments", "Back", "acme/api", "main", &[])).unwrap_err();
        assert!(err.to_string().starts_with("crews.Back: invalid crew name"), "{err}");

        let err = Fleet::try_from(spec("payments", "backend", "acme/api", "main", &["Bob"])).unwrap_err();
        assert!(err.to_string().starts_with("crews.backend.agents.Bob: invalid agent name"), "{err}");
    }

    #[test]
    fn invalid_repo_and_empty_ref_report_their_paths() {
        let err = Fleet::try_from(spec("payments", "backend", "nope", "main", &[])).unwrap_err();
        assert_eq!(err.to_string(), "crews.backend.repo: invalid repo \"nope\": expected owner/name or a clone URL");

        let err = Fleet::try_from(spec("payments", "backend", "acme/api", "", &[])).unwrap_err();
        assert_eq!(err.to_string(), "crews.backend.ref: must not be empty");
    }

    #[test]
    fn round_trips_back_to_the_wire_type() {
        let original = spec("payments", "backend", "acme/api", "main", &["alice"]);
        let back: FleetSpec = Fleet::try_from(original.clone()).unwrap().into();
        assert_eq!(back.name, original.name);
        // repo is normalized to the https clone URL on the way back
        assert_eq!(back.crews["backend"].repo, "https://github.com/acme/api.git");
        assert_eq!(back.crews["backend"].agents, original.crews["backend"].agents);
    }
}
```

`lib.rs`:
```rust
//! Domain model and ports (spec §3). No I/O lives here.

pub mod fleet;
pub mod name;
pub mod repo;

pub use fleet::{Crew, Fleet, FleetError};
pub use name::{AgentId, AgentName, CrewName, FleetName, NameError};
pub use repo::{RepoError, RepoRef};
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-core`
Expected: compile errors.

- [ ] **Step 3: Implement `repo.rs`**

Prepend to `crates/balerix-core/src/repo.rs`:
```rust
//! Repository references. GitHub gets first-class treatment (spec: every
//! agent has a git repo, with first-class GitHub support); anything else
//! is passed to git verbatim.

use std::fmt;

/// Why a string is not a repository reference.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid repo {value:?}: {reason}")]
pub struct RepoError {
    pub value: String,
    pub reason: &'static str,
}

/// Where a crew's code lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoRef {
    GitHub { owner: String, name: String },
    Url(String),
}

impl RepoRef {
    /// Accepts `owner/name`, `https://github.com/owner/name[.git]`,
    /// `git@github.com:owner/name[.git]`, or any other `scheme://` / `user@host:` URL.
    pub fn parse(s: &str) -> Result<Self, RepoError> {
        let err = |reason| RepoError { value: s.to_string(), reason };
        if s.is_empty() {
            return Err(err("empty"));
        }
        if let Some(rest) = s.strip_prefix("https://github.com/") {
            return parse_slug(rest.trim_end_matches(".git")).ok_or_else(|| err("expected owner/name or a clone URL"));
        }
        if let Some(rest) = s.strip_prefix("git@github.com:") {
            return parse_slug(rest.trim_end_matches(".git")).ok_or_else(|| err("expected owner/name or a clone URL"));
        }
        if s.contains("://") || (s.contains('@') && s.contains(':')) {
            return Ok(Self::Url(s.to_string()));
        }
        parse_slug(s).ok_or_else(|| err("expected owner/name or a clone URL"))
    }

    /// URL to hand to `git clone`.
    pub fn clone_url(&self) -> String {
        match self {
            Self::GitHub { owner, name } => format!("https://github.com/{owner}/{name}.git"),
            Self::Url(u) => u.clone(),
        }
    }

    /// `owner/name` for GitHub repos; `None` otherwise.
    pub fn github_slug(&self) -> Option<String> {
        match self {
            Self::GitHub { owner, name } => Some(format!("{owner}/{name}")),
            Self::Url(_) => None,
        }
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.clone_url())
    }
}

fn parse_slug(s: &str) -> Option<RepoRef> {
    let (owner, name) = s.split_once('/')?;
    let ok = |part: &str| {
        !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (ok(owner) && ok(name)).then(|| RepoRef::GitHub { owner: owner.to_string(), name: name.to_string() })
}
```

- [ ] **Step 4: Implement `fleet.rs`**

Prepend to `crates/balerix-core/src/fleet.rs`:
```rust
//! The domain fleet: the wire `FleetSpec` after every name and repo has
//! been validated ("parse, don't validate").

use std::collections::BTreeMap;

use balerix_api::{AgentSettings, CrewSpec, FleetSpec, GitSettings};

use crate::name::{AgentName, CrewName, FleetName, NameError};
use crate::repo::{RepoError, RepoRef};

/// A validated fleet.
#[derive(Debug, Clone, PartialEq)]
pub struct Fleet {
    pub name: FleetName,
    pub crews: BTreeMap<CrewName, Crew>,
}

/// A validated crew.
#[derive(Debug, Clone, PartialEq)]
pub struct Crew {
    pub repo: RepoRef,
    pub git_ref: String,
    pub git: GitSettings,
    pub agents: BTreeMap<AgentName, AgentSettings>,
}

/// A spec that failed validation. The message always starts with the
/// config path of the offending value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FleetError {
    #[error("{path}: {source}")]
    InvalidName { path: String, source: NameError },
    #[error("{path}: {source}")]
    InvalidRepo { path: String, source: RepoError },
    #[error("{path}: must not be empty")]
    EmptyRef { path: String },
}

impl TryFrom<FleetSpec> for Fleet {
    type Error = FleetError;

    fn try_from(spec: FleetSpec) -> Result<Self, FleetError> {
        let name = FleetName::try_from(spec.name)
            .map_err(|source| FleetError::InvalidName { path: "name".to_string(), source })?;
        let mut crews = BTreeMap::new();
        for (crew_name, crew) in spec.crews {
            let path = format!("crews.{crew_name}");
            let crew_name = CrewName::try_from(crew_name)
                .map_err(|source| FleetError::InvalidName { path: path.clone(), source })?;
            crews.insert(crew_name, convert_crew(&path, crew)?);
        }
        Ok(Self { name, crews })
    }
}

fn convert_crew(path: &str, crew: CrewSpec) -> Result<Crew, FleetError> {
    let repo = RepoRef::parse(&crew.repo)
        .map_err(|source| FleetError::InvalidRepo { path: format!("{path}.repo"), source })?;
    if crew.git_ref.is_empty() {
        return Err(FleetError::EmptyRef { path: format!("{path}.ref") });
    }
    let mut agents = BTreeMap::new();
    for (agent_name, settings) in crew.agents {
        let agent_path = format!("{path}.agents.{agent_name}");
        let agent_name = AgentName::try_from(agent_name)
            .map_err(|source| FleetError::InvalidName { path: agent_path, source })?;
        agents.insert(agent_name, settings);
    }
    Ok(Crew { repo, git_ref: crew.git_ref, git: crew.git, agents })
}

impl From<Fleet> for FleetSpec {
    fn from(fleet: Fleet) -> Self {
        Self {
            name: fleet.name.into(),
            crews: fleet
                .crews
                .into_iter()
                .map(|(name, crew)| {
                    (
                        name.into(),
                        CrewSpec {
                            repo: crew.repo.clone_url(),
                            git_ref: crew.git_ref,
                            git: crew.git,
                            agents: crew.agents.into_iter().map(|(n, s)| (n.into(), s)).collect(),
                        },
                    )
                })
                .collect(),
        }
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-core`
Expected: 16 tests pass.

- [ ] **Step 6: Lint and commit**

Run: `mise run lint`

```bash
git add crates/balerix-core
git commit -m "Add domain Fleet with validated conversion from FleetSpec"
```

---

### Task 6: `balerix-config` — deep merge

**Files:**
- Create: `crates/balerix-config/Cargo.toml`, `crates/balerix-config/src/lib.rs`, `crates/balerix-config/src/error.rs`, `crates/balerix-config/src/merge.rs`

**Interfaces:**
- Produces:
  ```rust
  pub fn merge(base: &Value, overlay: &Value) -> Value
  pub fn merge_layers<'a>(layers: impl IntoIterator<Item = &'a Value>) -> Value   // left fold from {}
  pub fn strip_nulls(v: &Value) -> Value
  pub enum ConfigError { Io { path: PathBuf, source: io::Error }, Yaml(serde_norway::Error), Invalid { path: String, message: String }, Fleet(FleetError) }
  ```
  Invariant used by every later task: **the output of `merge` never contains a `null` map value.**

- [ ] **Step 1: Create the crate and write the failing tests**

`crates/balerix-config/Cargo.toml`:
```toml
[package]
name = "balerix-config"
description = "Fleet YAML parsing, layered merge and resolution"
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

[dev-dependencies]
insta = { workspace = true }
proptest = { workspace = true }
tempfile = { workspace = true }
pretty_assertions = { workspace = true }

[lints]
workspace = true
```

`crates/balerix-config/src/error.rs`:
```rust
//! Errors from parsing, merging and resolving fleet configuration.

use std::path::PathBuf;

/// Anything that can go wrong before a spec reaches the server. `Invalid`
/// and `Fleet` messages start with the config path of the offending value.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid YAML: {0}")]
    Yaml(#[from] serde_norway::Error),
    #[error("{path}: {message}")]
    Invalid { path: String, message: String },
    #[error(transparent)]
    Fleet(#[from] balerix_core::FleetError),
}
```

`crates/balerix-config/src/lib.rs`:
```rust
//! Turns a three-level fleet YAML file into a fully-resolved `FleetSpec`
//! (spec §5). Pure apart from reading the file and host defaults.

pub mod error;
pub mod merge;

pub use error::ConfigError;
pub use merge::{merge, merge_layers, strip_nulls};
```

`crates/balerix-config/src/merge.rs` (tests only for now):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn scalar_in_overlay_wins() {
        assert_eq!(merge(&json!({"a": 1}), &json!({"a": 2})), json!({"a": 2}));
        assert_eq!(merge(&json!({"a": "x"}), &json!({"a": true})), json!({"a": true}));
    }

    #[test]
    fn maps_deep_merge() {
        let base = json!({"claude": {"settings": {"model": "sonnet", "theme": "dark"}}});
        let over = json!({"claude": {"settings": {"model": "opus"}, "resume": true}});
        assert_eq!(
            merge(&base, &over),
            json!({"claude": {"settings": {"model": "opus", "theme": "dark"}, "resume": true}})
        );
    }

    #[test]
    fn lists_replace_never_concatenate() {
        assert_eq!(merge(&json!({"args": ["--a", "--b"]}), &json!({"args": ["--c"]})), json!({"args": ["--c"]}));
        assert_eq!(merge(&json!({"args": ["--a"]}), &json!({"args": []})), json!({"args": []}));
    }

    #[test]
    fn null_in_overlay_deletes_the_key() {
        assert_eq!(merge(&json!({"a": 1, "b": 2}), &json!({"a": null})), json!({"b": 2}));
        assert_eq!(
            merge(&json!({"env": {"RUST_LOG": "info", "FOO": "1"}}), &json!({"env": {"FOO": null}})),
            json!({"env": {"RUST_LOG": "info"}})
        );
    }

    #[test]
    fn deleting_a_map_then_adding_into_it_starts_fresh() {
        // fleet sets x.y, crew deletes x, agent sets x.z → only z survives
        let layers = [json!({"x": {"y": 1}}), json!({"x": null}), json!({"x": {"z": 2}})];
        assert_eq!(merge_layers(&layers), json!({"x": {"z": 2}}));
    }

    #[test]
    fn overlay_kind_change_replaces() {
        assert_eq!(merge(&json!({"a": {"b": 1}}), &json!({"a": 5})), json!({"a": 5}));
        assert_eq!(merge(&json!({"a": 5}), &json!({"a": {"b": 1}})), json!({"a": {"b": 1}}));
    }

    #[test]
    fn empty_overlay_returns_base_without_nulls() {
        assert_eq!(merge(&json!({"a": 1, "gone": null}), &json!({})), json!({"a": 1}));
    }

    #[test]
    fn merge_layers_folds_left_from_empty() {
        assert_eq!(merge_layers(std::iter::empty()), json!({}));
        assert_eq!(merge_layers(&[json!({"a": 1}), json!({"b": 2}), json!({"a": 3})]), json!({"a": 3, "b": 2}));
    }

    #[test]
    fn strip_nulls_removes_null_entries_at_every_depth() {
        assert_eq!(strip_nulls(&json!({"a": null, "b": {"c": null, "d": 1}, "e": [null]})), json!({"b": {"d": 1}, "e": [null]}));
    }

    fn arb_json() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i32>().prop_map(Value::from),
            "[a-z]{0,3}".prop_map(Value::String),
        ];
        leaf.prop_recursive(3, 32, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..3).prop_map(Value::Array),
                prop::collection::btree_map("[a-c]", inner, 0..4)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }

    /// Objects only — the top-level settings layers are always mappings.
    fn arb_object() -> impl Strategy<Value = Value> {
        prop::collection::btree_map("[a-c]", arb_json(), 0..4).prop_map(|m| Value::Object(m.into_iter().collect()))
    }

    fn has_null_map_value(v: &Value) -> bool {
        match v {
            Value::Object(m) => m.values().any(|x| x.is_null() || has_null_map_value(x)),
            Value::Array(a) => a.iter().any(has_null_map_value),
            _ => false,
        }
    }

    proptest! {
        #[test]
        fn output_never_holds_null_map_values(a in arb_json(), b in arb_json()) {
            prop_assert!(!has_null_map_value(&merge(&a, &b)));
        }

        #[test]
        fn empty_overlay_is_strip_nulls(a in arb_object()) {
            // Only meaningful for mappings: `{}` onto a scalar is an overlay that wins.
            prop_assert_eq!(merge(&a, &json!({})), strip_nulls(&a));
        }

        #[test]
        fn merging_with_self_is_strip_nulls(a in arb_json()) {
            prop_assert_eq!(merge(&a, &a), strip_nulls(&a));
        }

        #[test]
        fn reapplying_the_overlay_changes_nothing(a in arb_json(), b in arb_json()) {
            let once = merge(&a, &b);
            prop_assert_eq!(merge(&once, &b), once);
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-config`
Expected: compile errors — `merge` undefined.

- [ ] **Step 3: Implement**

Prepend to `crates/balerix-config/src/merge.rs`:
```rust
//! Layered merge of settings blocks (spec §5). Rules: maps deep-merge,
//! scalars overlay-wins, lists replace, an explicit `null` in the overlay
//! deletes the key. Output never contains a `null` map value.
//!
//! The operation is a left fold, not associative: `{x: null}` means "reset
//! x relative to everything below me", which only makes sense in order.

use serde_json::{Map, Value};

/// Merges `overlay` onto `base`.
pub fn merge(base: &Value, overlay: &Value) -> Value {
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            let mut out = Map::new();
            for (k, bv) in b {
                if !o.contains_key(k) && !bv.is_null() {
                    out.insert(k.clone(), strip_nulls(bv));
                }
            }
            for (k, ov) in o {
                if ov.is_null() {
                    continue; // delete
                }
                let merged = match b.get(k) {
                    Some(bv) => merge(bv, ov),
                    None => strip_nulls(ov),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        (_, Value::Null) => strip_nulls(base),
        (_, o) => strip_nulls(o),
    }
}

/// Folds `layers` left-to-right onto an empty map; later layers win.
pub fn merge_layers<'a>(layers: impl IntoIterator<Item = &'a Value>) -> Value {
    layers.into_iter().fold(Value::Object(Map::new()), |acc, layer| merge(&acc, layer))
}

/// Removes `null` map entries at every depth. Array elements are kept as-is.
pub fn strip_nulls(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), strip_nulls(v))).collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(strip_nulls).collect()),
        other => other.clone(),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-config`
Expected: 13 tests pass (9 unit + 4 property).

- [ ] **Step 5: Lint and commit**

Run: `mise run lint`

```bash
git add crates/balerix-config Cargo.lock
git commit -m "Add balerix-config with layered deep merge"
```

---

### Task 7: `balerix-config` — parse the fleet file

**Files:**
- Create: `crates/balerix-config/src/file.rs`
- Modify: `crates/balerix-config/src/lib.rs`

**Interfaces:**
- Consumes: `ConfigError` (Task 6), `balerix_api::{API_VERSION, GitSettings}`.
- Produces:
  ```rust
  pub struct FleetFile { pub api_version: String, pub kind: String, pub name: Option<String>, pub defaults: Value, pub crews: BTreeMap<String, CrewFile> }
  pub struct CrewFile { pub repo: String, pub git_ref: String /* "ref", default "main" */, pub git: GitSettings, pub defaults: Value, pub agents: BTreeMap<String, Value> }
  pub fn parse(yaml: &str) -> Result<FleetFile, ConfigError>
  pub fn read(path: &Path) -> Result<FleetFile, ConfigError>
  ```
  Settings layers (`defaults`, each agent) stay as raw `Value` so merge runs before typing.

- [ ] **Step 1: Write the failing tests**

`crates/balerix-config/src/file.rs` (tests only):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    const MINIMAL: &str = "apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  backend:\n    repo: acme/api\n";

    #[test]
    fn parses_minimal_file_with_defaults() {
        let f = parse(MINIMAL).unwrap();
        assert_eq!(f.api_version, "balerix/v1");
        assert_eq!(f.kind, "Fleet");
        assert_eq!(f.name, None);
        assert_eq!(f.defaults, json!({}));
        let crew = &f.crews["backend"];
        assert_eq!(crew.repo, "acme/api");
        assert_eq!(crew.git_ref, "main");
        assert!(crew.git.push);
        assert_eq!(crew.defaults, json!({}));
        assert!(crew.agents.is_empty());
    }

    #[test]
    fn keeps_settings_layers_as_raw_values() {
        let yaml = r#"
apiVersion: balerix/v1
kind: Fleet
name: payments
defaults:
  tools: { node: "22.11.0" }
crews:
  backend:
    repo: acme/api
    ref: develop
    git: { push: false, auth: none }
    defaults:
      tools: { python: "3.12.8" }
    agents:
      alice: {}
      bob:
        claude: { settings: { model: opus } }
        tools: { node: null }
"#;
        let f = parse(yaml).unwrap();
        assert_eq!(f.name.as_deref(), Some("payments"));
        assert_eq!(f.defaults, json!({"tools": {"node": "22.11.0"}}));
        let crew = &f.crews["backend"];
        assert_eq!(crew.git_ref, "develop");
        assert!(!crew.git.push);
        assert_eq!(crew.defaults, json!({"tools": {"python": "3.12.8"}}));
        assert_eq!(crew.agents["alice"], json!({}));
        assert_eq!(crew.agents["bob"], json!({"claude": {"settings": {"model": "opus"}}, "tools": {"node": null}}));
    }

    #[test]
    fn rejects_wrong_api_version_and_kind() {
        let err = parse("apiVersion: balerix/v2\nkind: Fleet\n").unwrap_err();
        assert_eq!(err.to_string(), "apiVersion: expected \"balerix/v1\", got \"balerix/v2\"");
        let err = parse("apiVersion: balerix/v1\nkind: Crew\n").unwrap_err();
        assert_eq!(err.to_string(), "kind: expected \"Fleet\", got \"Crew\"");
    }

    #[test]
    fn rejects_unknown_keys_on_balerix_owned_structs() {
        let err = parse("apiVersion: balerix/v1\nkind: Fleet\ncrew:\n  backend:\n    repo: acme/api\n").unwrap_err();
        assert!(err.to_string().contains("unknown field `crew`"), "{err}");
        let err = parse("apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  backend:\n    repo: acme/api\n    agent: {}\n").unwrap_err();
        assert!(err.to_string().contains("unknown field `agent`"), "{err}");
    }

    #[test]
    fn requires_repo_per_crew() {
        let err = parse("apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  backend: {}\n").unwrap_err();
        assert!(err.to_string().contains("missing field `repo`"), "{err}");
    }

    #[test]
    fn read_reports_the_path_on_io_error() {
        let err = read(std::path::Path::new("/definitely/not/here.yaml")).unwrap_err();
        assert!(err.to_string().starts_with("failed to read /definitely/not/here.yaml"), "{err}");
    }
}
```

`lib.rs` additions: `pub mod file;` and `pub use file::{CrewFile, FleetFile, parse, read};`

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-config`
Expected: compile errors.

- [ ] **Step 3: Implement**

Prepend to `crates/balerix-config/src/file.rs`:
```rust
//! The on-disk fleet file (spec §5): the three-level form the user writes.

use std::collections::BTreeMap;
use std::path::Path;

use balerix_api::{API_VERSION, GitSettings};
use serde::Deserialize;
use serde_json::Value;

use crate::ConfigError;

const KIND: &str = "Fleet";

/// A parsed but unresolved fleet file. Settings layers are raw values.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetFile {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Fleet-level settings layer.
    #[serde(default = "empty_object")]
    pub defaults: Value,
    #[serde(default)]
    pub crews: BTreeMap<String, CrewFile>,
}

/// One crew as written in the file.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrewFile {
    pub repo: String,
    #[serde(rename = "ref", default = "default_ref")]
    pub git_ref: String,
    #[serde(default)]
    pub git: GitSettings,
    /// Crew-level settings layer.
    #[serde(default = "empty_object")]
    pub defaults: Value,
    /// Agent name → agent-level settings layer.
    #[serde(default)]
    pub agents: BTreeMap<String, Value>,
}

/// Parses YAML text and checks `apiVersion` / `kind`.
pub fn parse(yaml: &str) -> Result<FleetFile, ConfigError> {
    let file: FleetFile = serde_norway::from_str(yaml)?;
    if file.api_version != API_VERSION {
        return Err(ConfigError::Invalid {
            path: "apiVersion".to_string(),
            message: format!("expected {API_VERSION:?}, got {:?}", file.api_version),
        });
    }
    if file.kind != KIND {
        return Err(ConfigError::Invalid {
            path: "kind".to_string(),
            message: format!("expected {KIND:?}, got {:?}", file.kind),
        });
    }
    Ok(file)
}

/// Reads and parses a fleet file from disk.
pub fn read(path: &Path) -> Result<FleetFile, ConfigError> {
    let yaml = std::fs::read_to_string(path)
        .map_err(|source| ConfigError::Io { path: path.to_path_buf(), source })?;
    parse(&yaml)
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn default_ref() -> String {
    "main".to_string()
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-config`
Expected: 19 tests pass.

- [ ] **Step 5: Lint and commit**

Run: `mise run lint`

```bash
git add crates/balerix-config
git commit -m "Parse the three-level fleet YAML file"
```

---

### Task 8: `balerix-config` — validate a resolved agent

**Files:**
- Create: `crates/balerix-config/src/validate.rs`
- Modify: `crates/balerix-config/src/lib.rs`

**Interfaces:**
- Consumes: `balerix_api::AgentSettings`, `ConfigError`.
- Produces:
  ```rust
  pub fn validate_agent(path: &str, settings: &AgentSettings) -> Result<(), ConfigError>
  pub fn is_exact_version(v: &str) -> bool
  pub const RESERVED_ENV_PREFIXES: &[&str]   // "HOME", "XDG_", "CLAUDE_CONFIG_DIR", "GH_CONFIG_DIR", "MISE_", "BALERIX_", "PATH"
  ```
  Checks: every `tools` value is exact; `claude.settings` is a map without `hooks`; `sandbox` and `flow` are maps; no `env` key is reserved or empty.

- [ ] **Step 1: Write the failing tests**

`crates/balerix-config/src/validate.rs` (tests only):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{AgentSettings, ClaudeSettings};
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn exact_versions_are_accepted() {
        for v in ["22.11.0", "3.7c", "2026.9.1", "0.75.0", "2.1.261", "1.0.0-rc.1", "v1.2.3", "8.30.1"] {
            assert!(is_exact_version(v), "{v:?} should be exact");
        }
    }

    #[test]
    fn fuzzy_versions_are_rejected() {
        for v in ["22", "22.11", "latest", "lts", "system", "22.x", "22.11.x", "22*", "prefix:22", "ref:main", "sub-1:latest", "path:/x", ""] {
            assert!(!is_exact_version(v), "{v:?} should be fuzzy");
        }
    }

    fn with_tools(tools: &[(&str, &str)]) -> AgentSettings {
        AgentSettings {
            tools: tools.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..AgentSettings::default()
        }
    }

    #[test]
    fn happy_path_passes() {
        let s = AgentSettings {
            claude: ClaudeSettings { settings: json!({"model": "opus"}), ..ClaudeSettings::default() },
            env: BTreeMap::from([("RUST_LOG".to_string(), "info".to_string())]),
            ..with_tools(&[("node", "22.11.0")])
        };
        validate_agent("crews.backend.agents.bob", &s).unwrap();
    }

    #[test]
    fn fuzzy_tool_version_error_names_the_path_and_hints_mise_latest() {
        let err = validate_agent("crews.backend.agents.bob", &with_tools(&[("node", "22")])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "crews.backend.agents.bob.tools.node: expected an exact version, got \"22\" (try: mise latest node@22)"
        );
    }

    #[test]
    fn hooks_in_claude_settings_are_rejected() {
        let s = AgentSettings {
            claude: ClaudeSettings { settings: json!({"hooks": {}}), ..ClaudeSettings::default() },
            ..AgentSettings::default()
        };
        let err = validate_agent("crews.c.agents.a", &s).unwrap_err();
        assert_eq!(err.to_string(), "crews.c.agents.a.claude.settings.hooks: balerix owns this key; configure hook behaviour via `flow` instead");
    }

    #[test]
    fn passthrough_blocks_must_be_maps() {
        let s = AgentSettings { sandbox: json!([1]), ..AgentSettings::default() };
        assert_eq!(validate_agent("p", &s).unwrap_err().to_string(), "p.sandbox: expected a mapping");
        let s = AgentSettings { flow: json!("x"), ..AgentSettings::default() };
        assert_eq!(validate_agent("p", &s).unwrap_err().to_string(), "p.flow: expected a mapping");
        let s = AgentSettings {
            claude: ClaudeSettings { settings: json!(3), ..ClaudeSettings::default() },
            ..AgentSettings::default()
        };
        assert_eq!(validate_agent("p", &s).unwrap_err().to_string(), "p.claude.settings: expected a mapping");
    }

    #[test]
    fn reserved_env_keys_are_rejected() {
        for key in ["HOME", "XDG_DATA_HOME", "CLAUDE_CONFIG_DIR", "GH_CONFIG_DIR", "MISE_DATA_DIR", "BALERIX_FLEET", "PATH"] {
            let s = AgentSettings { env: BTreeMap::from([(key.to_string(), "x".to_string())]), ..AgentSettings::default() };
            let err = validate_agent("p", &s).unwrap_err();
            assert_eq!(err.to_string(), format!("p.env.{key}: reserved; balerix sets this variable"), "for {key}");
        }
        let s = AgentSettings { env: BTreeMap::from([(String::new(), "x".to_string())]), ..AgentSettings::default() };
        assert_eq!(validate_agent("p", &s).unwrap_err().to_string(), "p.env: empty variable name");
    }
}
```

`lib.rs` additions: `pub mod validate;` and `pub use validate::{is_exact_version, validate_agent, RESERVED_ENV_PREFIXES};`

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-config`
Expected: compile errors.

- [ ] **Step 3: Implement**

Prepend to `crates/balerix-config/src/validate.rs`:
```rust
//! Client-side checks on a resolved agent (spec §5 "Validation"). Names and
//! repos are validated by `balerix_core::Fleet`; this covers everything else.

use balerix_api::AgentSettings;
use serde_json::Value;

use crate::ConfigError;

/// Environment variables balerix sets itself (spec §4 table); user `env`
/// may not override them. Entries ending in `_` are prefixes.
pub const RESERVED_ENV_PREFIXES: &[&str] =
    &["HOME", "XDG_", "CLAUDE_CONFIG_DIR", "GH_CONFIG_DIR", "MISE_", "BALERIX_", "PATH"];

/// Rejects mise's fuzzy forms: keywords, `prefix:`/`ref:`/`path:`/`sub-`
/// specs, wildcards, and bare `major` / `major.minor` numbers.
pub fn is_exact_version(v: &str) -> bool {
    if v.is_empty() || matches!(v, "latest" | "lts" | "system") {
        return false;
    }
    if v.ends_with(".x") || v.contains('*') || v.contains(':') {
        return false;
    }
    let numeric_parts = v.split('.').filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())).count();
    let total_parts = v.split('.').count();
    // "22" and "22.11" are fuzzy; "3.7c", "1.0.0-rc.1", "v1.2.3" are exact.
    !(numeric_parts == total_parts && total_parts < 3)
}

/// Validates one resolved settings block; `path` prefixes every message.
pub fn validate_agent(path: &str, settings: &AgentSettings) -> Result<(), ConfigError> {
    let invalid = |suffix: &str, message: String| ConfigError::Invalid { path: format!("{path}.{suffix}"), message };

    for (tool, version) in &settings.tools {
        if !is_exact_version(version) {
            return Err(invalid(
                &format!("tools.{tool}"),
                format!("expected an exact version, got {version:?} (try: mise latest {tool}@{version})"),
            ));
        }
    }

    let Value::Object(claude_settings) = &settings.claude.settings else {
        return Err(invalid("claude.settings", "expected a mapping".to_string()));
    };
    if claude_settings.contains_key("hooks") {
        return Err(invalid(
            "claude.settings.hooks",
            "balerix owns this key; configure hook behaviour via `flow` instead".to_string(),
        ));
    }
    if !settings.sandbox.is_object() {
        return Err(invalid("sandbox", "expected a mapping".to_string()));
    }
    if !settings.flow.is_object() {
        return Err(invalid("flow", "expected a mapping".to_string()));
    }

    for key in settings.env.keys() {
        if key.is_empty() {
            return Err(invalid("env", "empty variable name".to_string()));
        }
        let reserved = RESERVED_ENV_PREFIXES
            .iter()
            .any(|r| if r.ends_with('_') { key.starts_with(r) } else { key == r });
        if reserved {
            return Err(invalid(&format!("env.{key}"), "reserved; balerix sets this variable".to_string()));
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-config`
Expected: 26 tests pass.

- [ ] **Step 5: Lint and commit**

Run: `mise run lint`

```bash
git add crates/balerix-config
git commit -m "Validate resolved agent settings: exact versions, owned keys, reserved env"
```

---

### Task 9: `balerix-config` — resolve layers into a `FleetSpec`, with golden snapshots

**Files:**
- Create: `crates/balerix-config/src/resolve.rs`
- Create: `examples/payments.yaml` (repo root — doubles as the README example), `crates/balerix-config/tests/fixtures/overrides.yaml`
- Create: `crates/balerix-config/tests/resolve_golden.rs`
- Modify: `crates/balerix-config/src/lib.rs`

**Interfaces:**
- Consumes: `FleetFile`, `CrewFile` (Task 7); `merge_layers` (Task 6); `validate_agent` (Task 8); `balerix_core::Fleet` (Task 5); `balerix_api::{FleetSpec, CrewSpec, AgentSettings}`.
- Produces:
  ```rust
  pub struct ResolveOptions { pub name_override: Option<String>, pub host_claude_settings: Option<Value> }
  pub fn resolve(file: &FleetFile, opts: &ResolveOptions) -> Result<FleetSpec, ConfigError>
  ```
  Layer order per agent: host `claude.settings` → `defaults` → `crews.<c>.defaults` → `crews.<c>.agents.<a>`.

- [ ] **Step 1: Write the fixtures**

`examples/payments.yaml` (the spec §5 example, exact versions):
```yaml
apiVersion: balerix/v1
kind: Fleet
name: payments
defaults:
  claude:
    settings: { model: sonnet, permissions: { allow: ["Bash(git *)"] } }
    args: ["--verbose"]
    resume: true
  sandbox:
    network: { mode: allow }
  tools: { node: "22.11.0" }
  env: { RUST_LOG: info }
  runner: { type: tmux }
  flow: {}
crews:
  backend:
    repo: acme/payments-api
    ref: main
    git: { push: true, auth: gh }
    defaults:
      tools: { python: "3.12.8" }
    agents:
      alice: {}
      bob: { claude: { settings: { model: opus } } }
```

`crates/balerix-config/tests/fixtures/overrides.yaml`:
```yaml
apiVersion: balerix/v1
kind: Fleet
name: overrides
defaults:
  claude: { args: ["--verbose", "--debug"] }
  tools: { node: "22.11.0", python: "3.12.8" }
  env: { RUST_LOG: info, FOO: bar }
crews:
  web:
    repo: https://github.com/acme/web.git
    defaults:
      tools: { python: null }   # crew deletes python
      env: { FOO: null }        # and FOO
    agents:
      carol:
        claude: { args: ["--quiet"] }   # list replaces, never appends
      dave:
        tools: { python: "3.13.1" }     # re-adds python after the crew deleted it
```

- [ ] **Step 2: Write the failing unit tests**

`crates/balerix-config/src/resolve.rs` (tests only for now):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn file(yaml: &str) -> FleetFile {
        parse(yaml).unwrap()
    }

    fn opts() -> ResolveOptions {
        ResolveOptions { name_override: None, host_claude_settings: None }
    }

    const BASE: &str = "apiVersion: balerix/v1\nkind: Fleet\nname: f\ncrews:\n  c:\n    repo: o/r\n    agents:\n      a: {}\n";

    #[test]
    fn name_override_beats_file_name_and_missing_name_errors() {
        let spec = resolve(&file(BASE), &ResolveOptions { name_override: Some("other".into()), ..opts() }).unwrap();
        assert_eq!(spec.name, "other");

        let no_name = "apiVersion: balerix/v1\nkind: Fleet\ncrews: {}\n";
        let err = resolve(&file(no_name), &opts()).unwrap_err();
        assert_eq!(err.to_string(), "name: required; set `name` in the file or pass --name");
    }

    #[test]
    fn layers_merge_fleet_then_crew_then_agent() {
        let yaml = r#"
apiVersion: balerix/v1
kind: Fleet
name: f
defaults:
  claude: { settings: { model: sonnet, theme: dark } }
  tools: { node: "22.11.0" }
crews:
  c:
    repo: o/r
    defaults:
      claude: { settings: { model: opus } }
    agents:
      a: { tools: { node: null, go: "1.23.4" } }
"#;
        let spec = resolve(&file(yaml), &opts()).unwrap();
        let a = &spec.crews["c"].agents["a"];
        assert_eq!(a.claude.settings, json!({"model": "opus", "theme": "dark"}));
        assert_eq!(a.tools, std::collections::BTreeMap::from([("go".to_string(), "1.23.4".to_string())]));
    }

    #[test]
    fn host_claude_settings_sit_beneath_fleet_defaults() {
        let yaml = "apiVersion: balerix/v1\nkind: Fleet\nname: f\ndefaults:\n  claude: { settings: { model: sonnet } }\ncrews:\n  c:\n    repo: o/r\n    agents:\n      a: {}\n";
        let host = json!({"model": "haiku", "theme": "dark"});
        let spec = resolve(&file(yaml), &ResolveOptions { host_claude_settings: Some(host), ..opts() }).unwrap();
        assert_eq!(spec.crews["c"].agents["a"].claude.settings, json!({"model": "sonnet", "theme": "dark"}));
    }

    #[test]
    fn crew_fields_are_carried_through() {
        let yaml = "apiVersion: balerix/v1\nkind: Fleet\nname: f\ncrews:\n  c:\n    repo: o/r\n    ref: dev\n    git: { push: false }\n    agents: {}\n";
        let spec = resolve(&file(yaml), &opts()).unwrap();
        assert_eq!(spec.crews["c"].repo, "o/r");
        assert_eq!(spec.crews["c"].git_ref, "dev");
        assert!(!spec.crews["c"].git.push);
    }

    #[test]
    fn unknown_key_in_an_agent_layer_reports_the_agent_path() {
        let yaml = "apiVersion: balerix/v1\nkind: Fleet\nname: f\ncrews:\n  c:\n    repo: o/r\n    agents:\n      a: { claud: {} }\n";
        let err = resolve(&file(yaml), &opts()).unwrap_err();
        assert!(err.to_string().starts_with("crews.c.agents.a: unknown field `claud`"), "{err}");
    }

    #[test]
    fn non_mapping_layers_are_rejected_with_their_path() {
        let yaml = "apiVersion: balerix/v1\nkind: Fleet\nname: f\ndefaults: [1]\ncrews: {}\n";
        assert_eq!(resolve(&file(yaml), &opts()).unwrap_err().to_string(), "defaults: expected a mapping");
        let yaml = "apiVersion: balerix/v1\nkind: Fleet\nname: f\ncrews:\n  c:\n    repo: o/r\n    agents:\n      a: 3\n";
        assert_eq!(resolve(&file(yaml), &opts()).unwrap_err().to_string(), "crews.c.agents.a: expected a mapping");
    }

    #[test]
    fn validation_and_name_errors_propagate_with_paths() {
        let yaml = "apiVersion: balerix/v1\nkind: Fleet\nname: f\ncrews:\n  c:\n    repo: o/r\n    agents:\n      a: { tools: { node: \"22\" } }\n";
        assert!(resolve(&file(yaml), &opts()).unwrap_err().to_string().starts_with("crews.c.agents.a.tools.node:"));
        let yaml = "apiVersion: balerix/v1\nkind: Fleet\nname: f\ncrews:\n  c:\n    repo: o/r\n    agents:\n      Bad: {}\n";
        assert!(resolve(&file(yaml), &opts()).unwrap_err().to_string().starts_with("crews.c.agents.Bad: invalid agent name"));
    }
}
```

`lib.rs` additions: `pub mod resolve;` and `pub use resolve::{resolve, ResolveOptions};`

- [ ] **Step 3: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-config`
Expected: compile errors.

- [ ] **Step 4: Implement**

Prepend to `crates/balerix-config/src/resolve.rs`:
```rust
//! Resolution (spec §5): fold the settings layers for every agent, type the
//! result, validate it, and hand back a `FleetSpec` with no inheritance left.

use std::collections::BTreeMap;

use balerix_api::{AgentSettings, CrewSpec, FleetSpec};
use balerix_core::Fleet;
use serde_json::{Value, json};

use crate::file::FleetFile;
use crate::merge::merge_layers;
use crate::validate::validate_agent;
use crate::ConfigError;

/// Inputs to resolution that do not come from the file itself.
#[derive(Debug, Clone, Default)]
pub struct ResolveOptions {
    /// `--name` on the CLI; wins over the file's `name`.
    pub name_override: Option<String>,
    /// The host's `~/.claude/settings.json`, layered beneath `defaults`.
    pub host_claude_settings: Option<Value>,
}

/// Resolves every agent and validates the result.
pub fn resolve(file: &FleetFile, opts: &ResolveOptions) -> Result<FleetSpec, ConfigError> {
    let name = opts.name_override.clone().or_else(|| file.name.clone()).ok_or_else(|| ConfigError::Invalid {
        path: "name".to_string(),
        message: "required; set `name` in the file or pass --name".to_string(),
    })?;
    let host_layer = opts.host_claude_settings.as_ref().map(|s| json!({ "claude": { "settings": s } }));
    expect_mapping("defaults", &file.defaults)?;

    let mut crews = BTreeMap::new();
    for (crew_name, crew) in &file.crews {
        let crew_path = format!("crews.{crew_name}");
        expect_mapping(&format!("{crew_path}.defaults"), &crew.defaults)?;

        let mut agents = BTreeMap::new();
        for (agent_name, layer) in &crew.agents {
            let agent_path = format!("{crew_path}.agents.{agent_name}");
            expect_mapping(&agent_path, layer)?;
            let merged = merge_layers(host_layer.iter().chain([&file.defaults, &crew.defaults, layer]));
            let settings: AgentSettings = serde_json::from_value(merged)
                .map_err(|e| ConfigError::Invalid { path: agent_path.clone(), message: e.to_string() })?;
            validate_agent(&agent_path, &settings)?;
            agents.insert(agent_name.clone(), settings);
        }
        crews.insert(
            crew_name.clone(),
            CrewSpec { repo: crew.repo.clone(), git_ref: crew.git_ref.clone(), git: crew.git.clone(), agents },
        );
    }

    let spec = FleetSpec { name, crews };
    Fleet::try_from(spec.clone())?; // names and repos
    Ok(spec)
}

/// A settings layer must be a mapping (or absent/null, which merge treats as empty).
fn expect_mapping(path: &str, v: &Value) -> Result<(), ConfigError> {
    if v.is_object() || v.is_null() {
        Ok(())
    } else {
        Err(ConfigError::Invalid { path: path.to_string(), message: "expected a mapping".to_string() })
    }
}
```

- [ ] **Step 5: Run the unit tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-config`
Expected: 33 tests pass.

- [ ] **Step 6: Write the golden tests**

`crates/balerix-config/tests/resolve_golden.rs`:
```rust
//! Golden snapshots of resolved specs. Review every `.snap.new` by hand
//! before `cargo insta accept` — the values to check are listed in the plan.

use balerix_config::{ResolveOptions, parse, resolve};
use serde_json::json;

fn read(path: &str) -> String {
    let full = format!("{}/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{full}: {e}"))
}

#[test]
fn payments_example_resolves_to_known_spec() {
    let file = parse(&read("../../examples/payments.yaml")).unwrap();
    let host = json!({ "theme": "dark", "model": "haiku" });
    let spec = resolve(&file, &ResolveOptions { name_override: None, host_claude_settings: Some(host) }).unwrap();
    insta::assert_yaml_snapshot!("payments", spec);
}

#[test]
fn overrides_exercise_null_delete_and_list_replace() {
    let file = parse(&read("tests/fixtures/overrides.yaml")).unwrap();
    let spec = resolve(&file, &ResolveOptions::default()).unwrap();
    insta::assert_yaml_snapshot!("overrides", spec);
}
```

- [ ] **Step 7: Run, review the new snapshots, accept**

Run: `mise x -- cargo nextest run -p balerix-config --test resolve_golden`
Expected: both tests FAIL with "snapshot assertion for 'payments' failed … new snapshot" (no committed reference yet).

Open `crates/balerix-config/tests/snapshots/resolve_golden__payments.snap.new` and confirm:
- `alice.claude.settings` has `model: sonnet` (fleet beat host `haiku`), `theme: dark` (from host), `permissions.allow: ["Bash(git *)"]`; `args: ["--verbose"]`; `resume: true`
- `bob.claude.settings.model: opus`; everything else identical to alice
- both agents: `tools: {node: 22.11.0, python: 3.12.8}`, `env: {RUST_LOG: info}`, `runner: {type: tmux}`, `sandbox: {network: {mode: allow}}`, `flow: {}`
- crew: `repo: acme/payments-api`, `ref: main`, `git: {push: true, auth: gh}`

Open `…/resolve_golden__overrides.snap.new` and confirm:
- `carol`: `tools: {node: 22.11.0}` (python deleted), `env: {RUST_LOG: info}` (FOO deleted), `claude.args: ["--quiet"]`
- `dave`: `tools: {node: 22.11.0, python: 3.13.1}`, `claude.args: ["--verbose", "--debug"]`, `env: {RUST_LOG: info}`

If anything differs, fix the code — not the snapshot. Then:

Run: `mise x -- cargo insta accept && mise x -- cargo nextest run -p balerix-config`
Expected: 35 tests pass; two `.snap` files now exist under `tests/snapshots/`.

- [ ] **Step 8: Lint and commit**

Run: `mise run lint`

```bash
git add crates/balerix-config examples/payments.yaml
git commit -m "Resolve fleet layers into a FleetSpec with golden snapshots"
```

---

### Task 10: `balerix-config` — host defaults discovery

**Files:**
- Create: `crates/balerix-config/src/host.rs`
- Modify: `crates/balerix-config/src/lib.rs`

**Interfaces:**
- Consumes: `ConfigError`; `balerix_api::CredentialBundle`.
- Produces:
  ```rust
  pub struct HostPaths { pub claude_dir: PathBuf, pub claude_json: PathBuf, pub gh_hosts: PathBuf }
  impl HostPaths {
      pub fn discover() -> Result<HostPaths, ConfigError>;                                  // uses std::env
      pub fn from_env(home: &Path, env: impl Fn(&str) -> Option<OsString>) -> HostPaths;   // testable core
  }
  pub struct HostDefaults { pub claude_settings: Option<Value>, pub credentials: CredentialBundle }
  pub fn load(paths: &HostPaths) -> Result<HostDefaults, ConfigError>
  ```
  Missing files yield `None`; malformed files are errors naming the path. `claude_account` carries only `oauthAccount` and `hasCompletedOnboarding`.

- [ ] **Step 1: Write the failing tests**

`crates/balerix-config/src/host.rs` (tests only):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::ffi::OsString;
    use std::fs;

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    #[test]
    fn from_env_defaults_to_dotfiles_under_home() {
        let p = HostPaths::from_env(Path::new("/h"), no_env);
        assert_eq!(p.claude_dir, PathBuf::from("/h/.claude"));
        assert_eq!(p.claude_json, PathBuf::from("/h/.claude.json"));
        assert_eq!(p.gh_hosts, PathBuf::from("/h/.config/gh/hosts.yml"));
    }

    #[test]
    fn from_env_honours_claude_config_dir_gh_config_dir_and_xdg() {
        let env = |k: &str| match k {
            "CLAUDE_CONFIG_DIR" => Some(OsString::from("/cc")),
            "GH_CONFIG_DIR" => Some(OsString::from("/ghc")),
            _ => None,
        };
        let p = HostPaths::from_env(Path::new("/h"), env);
        assert_eq!(p.claude_dir, PathBuf::from("/cc"));
        assert_eq!(p.gh_hosts, PathBuf::from("/ghc/hosts.yml"));

        let env = |k: &str| (k == "XDG_CONFIG_HOME").then(|| OsString::from("/xdg"));
        assert_eq!(HostPaths::from_env(Path::new("/h"), env).gh_hosts, PathBuf::from("/xdg/gh/hosts.yml"));
    }

    fn paths_in(dir: &Path) -> HostPaths {
        HostPaths {
            claude_dir: dir.join(".claude"),
            claude_json: dir.join(".claude.json"),
            gh_hosts: dir.join("gh/hosts.yml"),
        }
    }

    #[test]
    fn missing_files_yield_none_everywhere() {
        let tmp = tempfile::tempdir().unwrap();
        let d = load(&paths_in(tmp.path())).unwrap();
        assert_eq!(d.claude_settings, None);
        assert_eq!(d.credentials, balerix_api::CredentialBundle::default());
    }

    #[test]
    fn loads_settings_credentials_account_fields_and_gh_token() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths_in(tmp.path());
        fs::create_dir_all(&p.claude_dir).unwrap();
        fs::create_dir_all(p.gh_hosts.parent().unwrap()).unwrap();
        fs::write(p.claude_dir.join("settings.json"), r#"{"model":"haiku","theme":"dark"}"#).unwrap();
        fs::write(p.claude_dir.join(".credentials.json"), r#"{"claudeAiOauth":{"accessToken":"fake-token"}}"#).unwrap();
        fs::write(&p.claude_json, r#"{"oauthAccount":{"emailAddress":"x@y.z"},"hasCompletedOnboarding":true,"numStartups":42}"#).unwrap();
        fs::write(&p.gh_hosts, "github.com:\n    oauth_token: gho_fake\n    user: someone\n").unwrap();

        let d = load(&p).unwrap();
        assert_eq!(d.claude_settings, Some(json!({"model": "haiku", "theme": "dark"})));
        assert_eq!(d.credentials.claude_credentials, Some(json!({"claudeAiOauth": {"accessToken": "fake-token"}})));
        assert_eq!(
            d.credentials.claude_account,
            Some(json!({"oauthAccount": {"emailAddress": "x@y.z"}, "hasCompletedOnboarding": true}))
        );
        assert_eq!(d.credentials.gh_token.as_deref(), Some("gho_fake"));
    }

    #[test]
    fn gh_token_falls_back_to_the_users_map() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths_in(tmp.path());
        fs::create_dir_all(p.gh_hosts.parent().unwrap()).unwrap();
        fs::write(&p.gh_hosts, "github.com:\n    users:\n        someone:\n            oauth_token: gho_user\n    user: someone\n").unwrap();
        assert_eq!(load(&p).unwrap().credentials.gh_token.as_deref(), Some("gho_user"));
    }

    #[test]
    fn malformed_json_is_an_error_naming_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths_in(tmp.path());
        fs::create_dir_all(&p.claude_dir).unwrap();
        fs::write(p.claude_dir.join("settings.json"), "{not json").unwrap();
        let err = load(&p).unwrap_err().to_string();
        assert!(err.contains("settings.json"), "{err}");
    }
}
```

`lib.rs` additions: `pub mod host;` and `pub use host::{HostDefaults, HostPaths};`

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-config`
Expected: compile errors.

- [ ] **Step 3: Implement**

Prepend to `crates/balerix-config/src/host.rs`:
```rust
//! Host defaults (spec §5): the client's own Claude settings and
//! credentials, used as the bottom layer and the credential bundle.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use balerix_api::CredentialBundle;
use serde_json::{Map, Value};

use crate::ConfigError;

/// Keys lifted from `~/.claude.json` into the credential bundle.
const ACCOUNT_KEYS: &[&str] = &["oauthAccount", "hasCompletedOnboarding"];

/// Where the host keeps Claude and gh state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPaths {
    pub claude_dir: PathBuf,
    pub claude_json: PathBuf,
    pub gh_hosts: PathBuf,
}

impl HostPaths {
    /// Resolves from the real environment.
    pub fn discover() -> Result<Self, ConfigError> {
        let home = std::env::home_dir().ok_or_else(|| ConfigError::Invalid {
            path: "host".to_string(),
            message: "cannot determine the home directory".to_string(),
        })?;
        Ok(Self::from_env(&home, |k| std::env::var_os(k)))
    }

    /// Pure resolution: `CLAUDE_CONFIG_DIR`, `GH_CONFIG_DIR`, `XDG_CONFIG_HOME`
    /// are honoured in that order of specificity, then dotfiles under `home`.
    pub fn from_env(home: &Path, env: impl Fn(&str) -> Option<OsString>) -> Self {
        let claude_dir = env("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".claude"));
        let gh_dir = env("GH_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| env("XDG_CONFIG_HOME").map(|x| PathBuf::from(x).join("gh")))
            .unwrap_or_else(|| home.join(".config").join("gh"));
        Self { claude_dir, claude_json: home.join(".claude.json"), gh_hosts: gh_dir.join("hosts.yml") }
    }
}

/// What was found on the host. Absent files are `None`, never errors.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostDefaults {
    pub claude_settings: Option<Value>,
    pub credentials: CredentialBundle,
}

/// Reads host defaults from `paths`.
pub fn load(paths: &HostPaths) -> Result<HostDefaults, ConfigError> {
    let claude_settings = read_json_if_exists(&paths.claude_dir.join("settings.json"))?;
    let claude_credentials = read_json_if_exists(&paths.claude_dir.join(".credentials.json"))?;
    let claude_account = read_json_if_exists(&paths.claude_json)?.map(|v| pick(&v, ACCOUNT_KEYS));
    let gh_token = read_gh_token(&paths.gh_hosts)?;
    Ok(HostDefaults { claude_settings, credentials: CredentialBundle { claude_credentials, claude_account, gh_token } })
}

fn read_if_exists(path: &Path) -> Result<Option<String>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Io { path: path.to_path_buf(), source }),
    }
}

fn read_json_if_exists(path: &Path) -> Result<Option<Value>, ConfigError> {
    read_if_exists(path)?
        .map(|s| {
            serde_json::from_str(&s)
                .map_err(|e| ConfigError::Invalid { path: path.display().to_string(), message: format!("invalid JSON: {e}") })
        })
        .transpose()
}

/// `github.com.oauth_token`, else `github.com.users.<user>.oauth_token`.
fn read_gh_token(path: &Path) -> Result<Option<String>, ConfigError> {
    let Some(text) = read_if_exists(path)? else { return Ok(None) };
    let hosts: Value = serde_norway::from_str(&text)
        .map_err(|e| ConfigError::Invalid { path: path.display().to_string(), message: format!("invalid YAML: {e}") })?;
    let gh = &hosts["github.com"];
    let direct = gh["oauth_token"].as_str();
    let via_user = gh["user"].as_str().and_then(|u| gh["users"][u]["oauth_token"].as_str());
    Ok(direct.or(via_user).map(str::to_string))
}

fn pick(v: &Value, keys: &[&str]) -> Value {
    let mut out = Map::new();
    if let Value::Object(m) = v {
        for k in keys {
            if let Some(val) = m.get(*k) {
                out.insert((*k).to_string(), val.clone());
            }
        }
    }
    Value::Object(out)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-config`
Expected: 43 tests pass (41 unit + 2 golden).

- [ ] **Step 5: Lint and commit**

Run: `mise run lint`

```bash
git add crates/balerix-config
git commit -m "Discover host Claude settings, credentials and gh token"
```

---

### Task 11: `balerix` binary — `config resolve`

**Files:**
- Create: `crates/balerix/Cargo.toml`, `crates/balerix/src/main.rs`, `crates/balerix/src/cli.rs`, `crates/balerix/src/commands/mod.rs`, `crates/balerix/src/commands/config.rs`
- Create: `crates/balerix/tests/cli_config_resolve.rs`

**Interfaces:**
- Consumes: `balerix_config::{read, resolve, ResolveOptions, HostPaths, host::load}` (Tasks 7, 9, 10).
- Produces: the `balerix` binary with `balerix config resolve <FILE> [--name N] [--no-host-defaults] [--json]`. Exit 0 with the spec on stdout; exit 1 with `error: <path>: <message>` on stderr. Later phases add `serve`, `up`, `update`, `down` as sibling `Command` variants.

- [ ] **Step 1: Create the crate and write the failing CLI tests**

`crates/balerix/Cargo.toml`:
```toml
[package]
name = "balerix"
description = "Control plane and orchestrator for fleets of coding agents"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[[bin]]
name = "balerix"
path = "src/main.rs"

[dependencies]
balerix-api = { workspace = true }
balerix-config = { workspace = true }
anyhow = { workspace = true }
clap = { workspace = true }
serde_json = { workspace = true }
serde_norway = { workspace = true }

[dev-dependencies]
assert_cmd = { workspace = true }
predicates = { workspace = true }
tempfile = { workspace = true }

[lints]
workspace = true
```

`crates/balerix/tests/cli_config_resolve.rs`:
```rust
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

const PAYMENTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/payments.yaml");

/// A `balerix` command with an empty HOME so the real host is never read.
fn balerix(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_balerix"));
    cmd.env("HOME", home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("GH_CONFIG_DIR")
        .env_remove("XDG_CONFIG_HOME");
    cmd
}

fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
    let p = dir.join(name);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, content).unwrap();
    p
}

#[test]
fn resolves_the_example_to_yaml() {
    let home = tempfile::tempdir().unwrap();
    balerix(home.path())
        .args(["config", "resolve", PAYMENTS, "--no-host-defaults"])
        .assert()
        .success()
        .stdout(predicate::str::contains("name: payments"))
        .stdout(predicate::str::contains("opus"))
        .stdout(predicate::str::contains("22.11.0"))
        .stdout(predicate::str::contains("type: tmux"));
}

#[test]
fn json_flag_prints_parseable_json() {
    let home = tempfile::tempdir().unwrap();
    let out = balerix(home.path())
        .args(["config", "resolve", PAYMENTS, "--no-host-defaults", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["name"], "payments");
    assert_eq!(v["crews"]["backend"]["agents"]["bob"]["claude"]["settings"]["model"], "opus");
}

#[test]
fn name_flag_overrides_the_file() {
    let home = tempfile::tempdir().unwrap();
    balerix(home.path())
        .args(["config", "resolve", PAYMENTS, "--no-host-defaults", "--name", "renamed"])
        .assert()
        .success()
        .stdout(predicate::str::contains("name: renamed"));
}

#[test]
fn host_defaults_are_layered_beneath_the_file() {
    let home = tempfile::tempdir().unwrap();
    write(home.path(), ".claude/settings.json", r#"{"theme":"dark","model":"haiku"}"#);
    balerix(home.path())
        .args(["config", "resolve", PAYMENTS])
        .assert()
        .success()
        .stdout(predicate::str::contains("theme: dark"))
        .stdout(predicate::str::contains("model: sonnet"))
        .stdout(predicate::str::contains("haiku").not());
}

#[test]
fn invalid_config_exits_1_with_the_path_on_stderr() {
    let home = tempfile::tempdir().unwrap();
    let file = write(
        home.path(),
        "bad.yaml",
        "apiVersion: balerix/v1\nkind: Fleet\nname: f\ncrews:\n  backend:\n    repo: acme/api\n    agents:\n      alice: { tools: { node: \"22\" } }\n",
    );
    balerix(home.path())
        .args(["config", "resolve", file.to_str().unwrap(), "--no-host-defaults"])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::starts_with("error: crews.backend.agents.alice.tools.node: expected an exact version"));
}

#[test]
fn missing_file_exits_1() {
    let home = tempfile::tempdir().unwrap();
    balerix(home.path())
        .args(["config", "resolve", "/nope/fleet.yaml", "--no-host-defaults"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("failed to read /nope/fleet.yaml"));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix`
Expected: compile error — no `src/main.rs`.

- [ ] **Step 3: Implement the CLI**

`crates/balerix/src/cli.rs`:
```rust
//! Command-line surface. Every subcommand is a thin wrapper (spec §3):
//! parse args, call a library, print.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "balerix", version, about = "Control plane and orchestrator for fleets of coding agents")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Inspect fleet configuration without talking to the daemon.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the fully-resolved spec for a fleet file.
    Resolve(ResolveArgs),
}

#[derive(Debug, Args)]
pub struct ResolveArgs {
    /// Path to the fleet YAML file.
    pub file: PathBuf,
    /// Fleet name; overrides `name` in the file.
    #[arg(long)]
    pub name: Option<String>,
    /// Do not layer the host's ~/.claude/settings.json beneath the file.
    #[arg(long)]
    pub no_host_defaults: bool,
    /// Print JSON instead of YAML.
    #[arg(long)]
    pub json: bool,
}
```

`crates/balerix/src/commands/mod.rs`:
```rust
pub mod config;
```

`crates/balerix/src/commands/config.rs`:
```rust
//! `balerix config …`

use anyhow::Result;
use balerix_config::{HostPaths, ResolveOptions, host, read, resolve};

use crate::cli::ResolveArgs;

/// Resolves a fleet file and renders the spec. Credentials discovered on
/// the host are dropped here — this command never prints them.
pub fn resolve_command(args: &ResolveArgs) -> Result<String> {
    let file = read(&args.file)?;
    let host_claude_settings = if args.no_host_defaults {
        None
    } else {
        host::load(&HostPaths::discover()?)?.claude_settings
    };
    let spec = resolve(&file, &ResolveOptions { name_override: args.name.clone(), host_claude_settings })?;
    Ok(if args.json { serde_json::to_string_pretty(&spec)? + "\n" } else { serde_norway::to_string(&spec)? })
}
```

`crates/balerix/src/main.rs`:
```rust
//! `balerix` — CLI entry point.

mod cli;
mod commands;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command, ConfigCommand};

fn main() -> ExitCode {
    match run() {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<String> {
    let cli = Cli::parse();
    match cli.command {
        Command::Config { command: ConfigCommand::Resolve(args) } => commands::config::resolve_command(&args),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix`
Expected: 6 tests pass.

Run: `mise x -- cargo run -q -p balerix -- config resolve examples/payments.yaml --no-host-defaults | head -20`
Expected: YAML starting `name: payments` with `crews:` → `backend:` → `agents:` → `alice:` fully expanded.

- [ ] **Step 5: Lint and commit**

Run: `mise run lint`

```bash
git add crates/balerix Cargo.lock
git commit -m "Add balerix CLI with config resolve"
```

---

### Task 12: Navigability — ARCHITECTURE.md, README, AGENTS.md, spec touch-ups, verified onboarding

**Files:**
- Modify: `ARCHITECTURE.md`, `README.md`, `AGENTS.md`
- Create: `renovate.json`, `.github/workflows/ci.yml`
- Modify: `docs/superpowers/specs/2026-09-05-balerix-architecture-design.md` (two corrections discovered while planning)

**Interfaces:** none (documentation). Single-source rule: point at task names and files; never re-spell commands or mirror the tree.

- [ ] **Step 1: Write `ARCHITECTURE.md`** (decisions, not layout)

```markdown
# Architecture

Full design: `docs/superpowers/specs/2026-09-05-balerix-architecture-design.md`.
This page is the short map — decisions a newcomer would otherwise re-derive.

## Start here
`crates/balerix/src/main.rs` is the only entry point. Every subcommand is a thin
wrapper: parse args → call a library crate → print. `config resolve` is the
first one; `serve`, `up`, `update`, `down` follow in later phases.

Mental model: **balerix is a config generator and process launcher.** It turns a
fleet YAML file into one fully-resolved settings block per agent, then (later
phases) materializes each agent as a tmux window running
`nono run → mise exec → claude` in its own `$HOME`, and reconciles what exists
against what was declared.

## The pieces
- `balerix-api` — serde wire types. A leaf; no logic. Anything that holds a secret
  hand-implements `Debug` and prints `<redacted>`.
- `balerix-core` — the domain: validated names (`FleetName`, …), `RepoRef`, the
  `Fleet` that `FleetSpec` converts into with `TryFrom`. Later: the reconciler and
  the ports (`AgentRunner`, `FleetStore`, `EventHandler`) adapters implement.
  Never does I/O, so it tests with fakes.
- `balerix-config` — YAML → resolved `FleetSpec`. Parses the three-level file,
  deep-merges settings layers as JSON values, types and validates each agent.
- `balerix` — the binary, and the only crate allowed to see both ports and
  adapters; it does the wiring.

## How it flows (Phase 1)
`read` (file.rs) → `resolve` (resolve.rs): for each agent fold
`host claude.settings → defaults → crew.defaults → agent` with `merge_layers`
(merge.rs), deserialize into `AgentSettings`, `validate_agent` (validate.rs),
then `Fleet::try_from` for names and repos → print.

## Non-obvious decisions
- **Merge is a left fold, not associative.** `null` means "delete relative to the
  layers below me"; that only has meaning in order. Tested by property
  (idempotent, overlay-dominant, never emits null).
- **Resolution happens client-side.** The daemon only ever sees resolved specs, so
  merge semantics cannot drift between client and server.
- **Exact tool versions only.** `tools: { node: "22" }` is rejected; an unpinned
  entry is a reproducibility bug (developer-environment skill).
- **`claude.settings.hooks` is balerix-owned.** Hook wiring is how the daemon
  hears from agents; users shape behaviour through `flow` instead.
- **Ports live in `balerix-core`, adapters depend on it, never on each other.**
  The future Kubernetes split cuts between `balerix-server` and
  `balerix-runtime`; `core` is shared.
```

- [ ] **Step 2: Rewrite `README.md`**

```markdown
# balerix

A control plane and orchestrator for fleets of coding agents (Claude Code
first), driven over an HTTPS API from a thin CLI. Agents run isolated — own
`$HOME`, own tools, own sandbox — as tmux windows grouped into crews that share
a repository.

## Quickstart
1. `mise trust && mise install` — pinned toolchain (Rust and every tool balerix shells out to).
2. `git config core.hooksPath .githooks` — enables the pre-commit tier.
3. `mise run check` — lint + tests; the same gate CI runs.
4. `mise x -- cargo run -q -p balerix -- config resolve examples/payments.yaml`
   — resolves the example fleet and prints every agent's merged settings.

## Where to look
- `ARCHITECTURE.md` — the map: pieces, flow, non-obvious decisions.
- `AGENTS.md` — conventions and gotchas for contributors (human or agent).
- `docs/superpowers/specs/` — the design; `docs/superpowers/plans/` — how it is being built.
- `docs/THREAT-MODEL.md` — what is protected, from whom, and what is out of scope.

## Status
Phase 1 (configuration) is complete. The daemon, tmux runner and `up`/`down`
are the next phases; see the spec's §11.
```

- [ ] **Step 3: Finalize `AGENTS.md`**

Replace the file with:
```markdown
# Balerix — agent instructions

Read `ARCHITECTURE.md` first. The design is in
`docs/superpowers/specs/2026-09-05-balerix-architecture-design.md`; the threat
model in `docs/THREAT-MODEL.md` — load it before touching anything that handles
credentials, hook input, or sandbox rules.

## Tasks (`mise run <task>`)
- `check` — lint + test; run before every commit.
- `lint`, `test`, `fmt`, `precommit`, `audit` — defined in `mise.toml`.

## Conventions
- Ports (`AgentRunner`, `FleetStore`, `EventHandler`) live in `balerix-core`;
  adapter crates implement them and never depend on each other. Only the
  `balerix` binary wires adapters to ports.
- Library crates return `thiserror` errors whose messages start with the config
  path (`crews.backend.agents.bob.tools.node: …`); only the binary uses `anyhow`.
- Every tool version — `mise.toml` and fleet `tools:` — is exact.
- Types holding secrets hand-implement `Debug` and print `<redacted>`. Secrets
  never go in argv, env, logs, or `config resolve` output.
- New Cargo dependencies are a deliberate decision: add to
  `[workspace.dependencies]` with an exact version and say why in the commit.

## Gotchas
- Run cargo through mise (`mise x -- cargo …`) or via a `mise run` task.
- insta snapshots: read the `.snap.new`, compare against the plan's expected
  values, then `mise x -- cargo insta accept`. Never blind-accept.
- Edition 2024 makes `std::env::set_var` unsafe and the workspace forbids
  `unsafe`; inject environment through parameters (see `HostPaths::from_env`).
- The merge is a left fold — don't "fix" its non-associativity.
```

- [ ] **Step 4: Correct the spec where planning found it wrong**

In `docs/superpowers/specs/2026-09-05-balerix-architecture-design.md`:

Replace (§10 Developer environment):
> Repo `mise.toml` pins **exact** versions of: `rust`, `cargo-nextest`, `cargo-insta`, `cargo-audit`, `cargo-deny`, `cargo-mutants`, `gitleaks`, `tmux`, `git`, `gh`, `nono`, `claude`.

with:
> Repo `mise.toml` pins **exact** versions of: `rust`, `cargo-nextest`, `cargo-insta`, `cargo-audit`, `cargo-deny`, `cargo-mutants`, `gitleaks`, `tmux`, `gh`, `nono`, `claude`. `git` is not in the mise registry and is a system prerequisite (the devcontainer image provides it).

Replace (§10 Testing table, property row):
> merge is associative and idempotent; `FleetSpec` serde round-trips

with:
> merge never emits `null`, is idempotent (`merge(a,a) = strip_nulls(a)`), and re-applying an overlay is a no-op; `FleetSpec` serde round-trips. (Merge is deliberately a left fold and *not* associative — `null` deletes relative to the layers beneath it.)

- [ ] **Step 5: Add Renovate and the CI workflow (PR tier + nightly audit)**

`renovate.json` — Renovate understands `mise.toml` and `Cargo.lock`; updates land as small PRs gated by CI (developer-environment: update on a cadence, not in a panic):
```json
{
  "$schema": "https://docs.renovatebot.com/renovate-schema.json",
  "extends": ["config:recommended"],
  "lockFileMaintenance": { "enabled": true, "schedule": ["before 6am on monday"] },
  "rangeStrategy": "pin"
}
```

`.github/workflows/ci.yml` — runs exactly the mise tasks, so CI and laptops cannot drift:
```yaml
name: ci
on:
  push: { branches: [main] }
  pull_request:
  schedule:
    - cron: "17 3 * * *"   # nightly tier
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: jdx/mise-action@v2
        with: { install: true, cache: true }
      - run: mise run check
  audit:
    if: github.event_name == 'schedule'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: jdx/mise-action@v2
        with: { install: true, cache: true }
      - run: mise run audit
```
This file cannot be exercised from the devcontainer; confirm the `check` job is green on the first push and fix forward if the runner image lacks a tool the `cargo:` backends need (`pkg-config`, `libssl-dev`).

- [ ] **Step 6: Verify onboarding by running it from a fresh clone**

```bash
tmp=$(mktemp -d) && git clone -q /workspace "$tmp/balerix" && cd "$tmp/balerix" \
  && mise trust && mise install && git config core.hooksPath .githooks \
  && mise run check \
  && mise x -- cargo run -q -p balerix -- config resolve examples/payments.yaml --no-host-defaults | head -5 \
  && rm -rf "$tmp"
```
Expected: `mise run check` passes; the resolve output begins `name: payments`. (Uncommitted work is invisible to the clone — commit first if this fails on a missing file.)

- [ ] **Step 7: Commit**

```bash
git add ARCHITECTURE.md README.md AGENTS.md renovate.json .github/workflows/ci.yml docs/superpowers/specs/2026-09-05-balerix-architecture-design.md
git commit -m "Document Phase 1: architecture map, quickstart, conventions, CI; correct spec"
```

---

### Task 13: Threat model (Tier 1)

**Files:**
- Create: `docs/THREAT-MODEL.md`

**Interfaces:** none. Follows the `security-practices` template; mitigations reference spec sections for controls that later phases implement, and code for the ones Phase 1 ships.

- [ ] **Step 1: Write `docs/THREAT-MODEL.md`**

```markdown
# Threat model — balerix

Tier-1 model per the security-practices skill; revisit when a trust boundary
changes (new runner, control/data-plane split, new credential type). Controls
marked *(planned §N)* are specified in the architecture spec and land in later
phases; the rest exist in code today.

## Assets
- **Claude credentials** (`~/.claude/.credentials.json`, account fields) — grant API access billed to the user.
- **gh OAuth token** — read/write access to the crew's repositories.
- **Agent isolation** — one agent must not read another agent's `$HOME`, session, or the host's `~/.claude`.
- **Fleet state integrity** — the daemon's record of what should be running.
- **Host integrity** — agents run arbitrary code; the sandbox is what stands between them and the host.

## Trust boundaries
- **CLI ↔ daemon** — resolved specs and credential bundles cross it; the user controls both ends today, but transport is treated as untrusted (future remote control plane).
- **Agent ↔ daemon (hook ingress)** — event JSON produced by a process running LLM-driven code. **Untrusted input.**
- **Daemon ↔ disk** — credentials at rest, fleet records.
- **Agent ↔ host** — filesystem and network from inside the sandbox.
- **Daemon ↔ external tools** — argv/config handed to `git`, `gh`, `mise`, `nono`, `tmux`, `claude`.
- **Cloned repositories** — a repo's own `.claude/` settings, hooks and scripts are attacker-controlled content.
- **Supply chain** — crates and pinned tools.

## Adversaries
- **A compromised or misbehaving agent** — can run any command the sandbox allows, emit arbitrary hook payloads, write anything into its worktree and the shared crew `.git`. Wants: credentials, other agents' data, host access.
- **A malicious repository** — controls files the agent reads and Claude's project-level settings. Wants: to steer the agent or exfiltrate credentials.
- **A local unprivileged process on the host** — can read world-readable files and connect to loopback ports. Wants: tokens, the admin bearer token.
- **A malicious dependency** — code executing at build or run time.

## In scope
- Credential exposure on disk, in logs, in `Debug` output, in argv/env.
- Cross-agent access to `$HOME`, sessions, sandbox escape via mis-granted paths.
- Forged or replayed hook events; hook payloads used to inject into other agents.
- Repo-supplied config overriding balerix-owned keys.
- Known CVEs in dependencies; secrets committed to the repo.

## Out of scope / accepted risks
- **Kernel-level sandbox escapes** — nono/Landlock is the control; balerix does not add a second sandbox.
- **Git isolation between agents of the same crew** — they share `.git` by design (spec D3, §4); agents in a crew trust each other.
- **Daemon down ⇒ Claude's HTTP hooks fail open for most events** — recorded in spec §10; the fleet is degraded, not compromised.
- **A user who runs `balerix` with real credentials on a host they do not trust** — same trust as running `claude` itself.
- **Multi-tenant use** — one user per daemon in this iteration.

## Mitigations
| Threat | Control | Where |
|---|---|---|
| Secrets in debug output / logs | `CredentialBundle` hand-implements `Debug` → `<redacted>`; hook payloads logged at `debug` only | `crates/balerix-api/src/credentials.rs`; *(planned §8)* |
| Secrets printed by `config resolve` | host credentials are loaded and discarded; only the spec is rendered | `crates/balerix/src/commands/config.rs` |
| Repo/user config overriding hook wiring | `claude.settings.hooks` rejected at validation; daemon re-owns the key when writing `settings.json` | `crates/balerix-config/src/validate.rs`; *(planned §6)* |
| User `env` clobbering isolation variables | reserved `HOME`, `XDG_*`, `CLAUDE_CONFIG_DIR`, `GH_CONFIG_DIR`, `MISE_*`, `BALERIX_*`, `PATH` rejected | `crates/balerix-config/src/validate.rs` |
| Malformed names reaching tmux/branch/paths | DNS-label validation on fleet/crew/agent names before anything is created | `crates/balerix-core/src/name.rs` |
| Credentials at rest | vault key 0600, XChaCha20-Poly1305, plaintext only while writing an agent's `.credentials.json` | *(planned §7)* |
| Forged hook events | per-agent secret, body size limit, timeout, per-agent rate limit, validation at the edge | *(planned §7–§8)* |
| Local process reaching the API | loopback bind, TLS, bearer token 0600 | *(planned §7)* |
| Sandbox mis-grants | generated nono profile with explicit read-only system paths and read-write `home/`+`workspace/`; `nono profile validate` before launch; user grants that conflict are rejected, not overridden | *(planned §4, §6)* |
| Secrets in argv / env / `launch.sh` | gh token only in `hosts.yml` 0600; Claude creds only in `.credentials.json`; argv arrays, shell-quoted `launch.sh` | *(planned §6, §10)* |
| Supply chain | exact-pinned `mise.toml`; committed `Cargo.lock`; `cargo audit` + `cargo deny` (`mise run audit`); `gitleaks` in `mise run precommit` | `mise.toml`, `deny.toml` |
```

- [ ] **Step 2: Check the links hold**

Run: `grep -o 'crates/[a-z/_.-]*\.rs' docs/THREAT-MODEL.md | sort -u | while read f; do test -f "$f" && echo "ok  $f" || echo "MISSING $f"; done`
Expected: every line starts with `ok`.

- [ ] **Step 3: Commit**

```bash
git add docs/THREAT-MODEL.md
git commit -m "Add Tier-1 threat model"
```

---

## Done when

- `mise run check` and `mise run audit` pass on a fresh clone.
- `balerix config resolve examples/payments.yaml` prints the resolved spec; the invalid-version example exits 1 with a path-prefixed error.
- 60+ tests across `balerix-api` (14), `balerix-core` (16), `balerix-config` (41 + 2 golden), `balerix` (6).
- `ARCHITECTURE.md`, `README.md`, `AGENTS.md`, `docs/THREAT-MODEL.md` exist and the spec corrections from Task 12 are committed.
- Next: Phase 2 plan (`balerix-runtime` adapters, `balerix-core` reconciler + ports, model-based tests) — written against this code.

## Deliberately deferred (spec §10 items not in this phase)

- **Fuzz targets** for the YAML parser and hook-event JSON: `cargo-fuzz` needs a nightly toolchain pinned alongside stable; add when the nightly CI tier gets its own job (Phase 3, when hook ingress exists).
- **`cargo mutants`** run: pinned now, scheduled once `balerix-core` has the reconciler worth auditing (Phase 2).
- **Sandbox-path conflict validation** (user `sandbox` grants overlapping balerix's required paths): needs the state-dir layout, so it lives in the runtime's `SandboxProfile` step (Phase 2), not client-side validation.
