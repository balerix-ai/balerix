# Spec L: Plugin-managed Fleets Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a plugin with the new `manage` capability apply and tear down a fleet from an unresolved fleet file, with the daemon resolving it and reading the operator's credentials through two new ports; mark such fleets with an owner the CLI cannot silently take over; and let an agent work on an existing remote branch through a per-agent `branch` setting.

**Architecture:** Two ports in `balerix-core` (`FleetResolver`, `CredentialSource`) are implemented once, in the binary, over `balerix-config`, and handed to the daemon in `Ports`; `balerix-server` never imports `balerix-config`. `Daemon` gains an explicit caller (`Admin { force }` or `Plugin(name)`) on apply and down, an `owner` on `FleetRecord` that the caller rule reads, a `manage_fleet` path (check name, resolve, load credentials, upsert) behind `PUT/DELETE /v1/plugin-host/fleets/{name}` gated on `Capability::Manage`, and a `sync_plugins` that downs the fleets a stopped plugin owned. `AgentSettings.branch`, validated as a git branch name in `balerix-api`, becomes both the worktree branch and its start point in the materializer. The SDK, its `FakeHost`, three protocol fixtures, the CLI (`list`, `status`, `down --force`, `plugin remove`) and `dev fake-plugin` follow.

**Tech Stack:** Rust 1.98 (edition 2024), axum, tokio, serde, `serde_path_to_error`, cargo-nextest, proptest, real git in `balerix-runtime` integration tests, the e2e under `target/tmp` with nono/tmux/mise.

**Spec:** `docs/superpowers/specs/2026-09-22-balerix-l-managed-fleets-design.md`

## Global Constraints

- Run cargo through mise: `mise x -- cargo …`, or `mise run <task>`. `mise run check` (lint + test) must pass at the end of every task; `mise run test-it` for Task 4; the e2e (`mise run e2e`) for Task 9.
- Ports live in `balerix-core`; `balerix-server` depends on `core` and `api` only and never imports `balerix-config` or `balerix-runtime`; only the `balerix` binary wires adapters to ports (AGENTS.md).
- Library crates return `thiserror` errors whose messages start with the config path (`crews.repo.agents.issue-12.branch: …`); only the binary uses `anyhow`.
- `unsafe_code = "forbid"`; clippy `all` warn, `unwrap_used`/`expect_used` warn outside tests. Never `std::env::set_var`.
- New dependencies land in the crate that uses them, at an exact version, and the commit says why. This plan adds exactly one: `serde_path_to_error = "0.1.20"` (the version `plugins/common/Cargo.toml` already pins) to the root `[workspace.dependencies]` and to `balerix-config`. `scripts/check-core-deps.sh` must still pass: it is a serde helper with no `reqwest` feature.
- Every wire-type change is backward compatible on read: `#[serde(default)]` for new fields, `skip_serializing_if` for new `Option`s, so an older `fleet.json` and an older CLI's `DownQuery` still load.
- Secrets never enter argv, env or logs. The plugin never sees the credential bundle: the route takes a file, the daemon reads the bundle.
- The three new fixtures make `docs/plugin-protocol/` hold 26 files; `crates/balerix-plugin-sdk/tests/conformance.rs` asserts the count and `docs/plugin-protocol.md` §6 states it in words. Change both.
- insta snapshots: read the `.snap.new`, compare against the expected values, then `mise x -- cargo insta accept`. Never blind-accept (no snapshot is expected to change in this plan).
- Commit after every task. The final PR title is `feat(core): plugin-managed fleets, owned records and a per-agent branch (Spec L)`; it touches `crates/balerix-api/` and `crates/balerix-plugin-sdk/`, so it releases core and every plugin (AGENTS.md).

## Review Focus

1. **A `branch` whose remote branch does not exist** must fail the agent's materialize step with git's message and retry at the resync cadence, never create a branch from nothing or fall back to the crew ref silently. Test: `a_missing_remote_branch_fails_the_worktree` in Task 4.
2. **A plugin `PUT` whose `file.name` differs from the path name** must be a 400 naming both, before any resolving. Test: `a_file_whose_name_disagrees_with_the_path_is_refused_before_resolving` in Task 5.
3. **A plugin `PUT` whose `file` is not an object, or has a key the file format does not know**, must be a 400 whose message starts with the offending key's path (`file` for the root). Tests: `from_value_names_the_offending_key` in Task 2 and the `file` shape cases in Task 6.
4. **An owned fleet loaded from `fleet.json` after a daemon restart** keeps its owner: admin `up`/`update` on it are 409, `down` needs `--force`. Test: `a_stored_owned_record_keeps_its_owner_after_a_restart` in Task 5.
5. **`PUT /v1/plugin-host/fleets/watch`** must never create a fleet (the static `fleets/watch` route shadows the name). Test: the `watch` case in Task 6, and `reserved_names_are_refused_by_the_manage_path` in Task 5.

---

## File Structure

**Created**

- `crates/balerix-api/src/branch.rs` — `check_branch_name`: the `git check-ref-format --branch` rule as a pure function with a reason.
- `crates/balerix-server/tests/manage_it.rs` — the two plugin-host routes and the admin-side owner rules over HTTP.
- `docs/plugin-protocol/fleet-put.json`, `fleet-put-rejected.json`, `fleet-delete.json` — the conformance fixtures.

**Modified**

- `crates/balerix-api/src/{plugin,record,request,settings,status,lib}.rs` — `Capability::Manage`, `FleetRecord.owner`, `FleetSummary.managed_by`, `DownQuery.force`, `SyncReport.{downed,down_failed}`, `AgentSettings.branch`.
- `crates/balerix-config/src/{file,validate,lib}.rs`, `Cargo.toml` — `Serialize` on the file types, `from_value`, `branch` validation.
- `crates/balerix-core/src/{ports,agent,fakes,lib}.rs` — the two ports, `ResolvedAgent::{branch,start_ref}`, `FakeResolver`, `FakeCredentials`.
- `crates/balerix-runtime/src/materializer.rs`, `tests/workspace_it.rs` — the start point of a worktree; the branch tests.
- `crates/balerix-server/src/{actor,daemon,api,plugin_api,testing,store}.rs`, `src/plugins/host.rs`, `tests/plugins_it.rs`, `tests/support/mod.rs` — `Ports` fields, `Caller`/`ApplyMode`, owner rules, `manage_fleet`, sync downs, the routes, the harness.
- `crates/balerix-plugin-sdk/src/{host,testing}.rs`, `tests/conformance.rs` — `apply_fleet`/`down_fleet`, `FakeHost` support, fixture replay.
- `crates/balerix/src/{wiring,cli,main}.rs`, `src/commands/{serve,fleet,plugin,dev}.rs`, `tests/cli_fleet.rs`, `tests/e2e.rs` — `HostResolver`, `down --force`, the owner in `list`/`status`, `plugin remove` downing owned fleets, the fake plugin's manage mode, the e2e journey.
- `docs/plugin-protocol.md`, `docs/THREAT-MODEL.md`, `ARCHITECTURE.md`, `AGENTS.md`, the Spec L file (§12 "Recorded at implementation").

---

### Task 1: The wire types (`balerix-api`)

**Files:**
- Create: `crates/balerix-api/src/branch.rs`
- Modify: `crates/balerix-api/src/plugin.rs:49-58` (`Capability`), `:121-127` (`SyncReport`); `crates/balerix-api/src/record.rs`; `crates/balerix-api/src/request.rs:16-33`; `crates/balerix-api/src/settings.rs:10-46`; `crates/balerix-api/src/status.rs:155-163`; `crates/balerix-api/src/lib.rs`
- Modify (compile fixes only): `crates/balerix/src/commands/plugin.rs:338-342` (a `SyncReport` literal), `crates/balerix/src/commands/fleet.rs:244-248` (a `DownQuery` literal)

**Interfaces:**
- Produces: `Capability::Manage` (wire `"manage"`); `FleetRecord { owner: Option<String>, .. }`, `FleetRecord::with_owner(spec, owner: Option<String>) -> FleetRecord`; `FleetSummary { managed_by: Option<String>, .. }`; `DownQuery { force: bool, .. }` with `to_query_string()` ending in `&force=<bool>`; `SyncReport { downed: Vec<String>, down_failed: Vec<String>, .. }`; `AgentSettings { branch: Option<String>, .. }`; `pub fn check_branch_name(name: &str) -> Result<(), String>` re-exported at the crate root.

- [ ] **Step 1: Write the failing tests for the four type changes**

In `crates/balerix-api/src/plugin.rs`, inside `mod tests`, change the assertion in `manifest_rejects_unknown_fields_and_capabilities` and add a report test:

```rust
        assert_eq!(
            serde_json::from_value::<Capability>(json!("manage")).unwrap(),
            Capability::Manage,
            "Spec L-1: the capability that gates the two fleet routes"
        );
        assert_eq!(
            serde_json::to_value(Capability::Manage).unwrap(),
            json!("manage")
        );
```

```rust
    /// A report from a daemon that predates Spec L has no `downed` lists.
    #[test]
    fn a_sync_report_without_the_downed_lists_still_loads() {
        let older: SyncReport = serde_json::from_value(json!({
            "installed": [], "stopped": ["x"], "unchanged": []
        }))
        .unwrap();
        assert!(older.downed.is_empty() && older.down_failed.is_empty());
        let r = SyncReport {
            downed: vec!["f".into()],
            down_failed: vec!["g: fleet task is gone".into()],
            ..SyncReport::default()
        };
        let back: SyncReport = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }
```

In `crates/balerix-api/src/record.rs` `mod tests`:

```rust
    /// Spec L §5: the plugin that applied a fleet, absent for the CLI's
    /// fleets and in every `fleet.json` written before Spec L.
    #[test]
    fn owner_is_absent_by_default_and_round_trips_when_set() {
        let r = FleetRecord::new(spec());
        assert_eq!(r.owner, None);
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("owner").is_none(), "no key when unowned: {v}");
        assert_eq!(r.summary().managed_by, None);
        let older: FleetRecord = serde_json::from_value(json!({
            "spec": { "name": "payments" }, "generation": 1, "desired": { "state": "up" },
            "status": { "generation": 1, "observed_generation": 1, "phase": "ready" }
        }))
        .unwrap();
        assert_eq!(older.owner, None, "a pre-Spec-L fleet.json loads");
        let owned = FleetRecord::with_owner(spec(), Some("github".into()));
        assert_eq!(owned.owner.as_deref(), Some("github"));
        assert_eq!(owned.generation, 0);
        let v = serde_json::to_value(&owned).unwrap();
        assert_eq!(v["owner"], "github");
        let back: FleetRecord = serde_json::from_value(v).unwrap();
        assert_eq!(back, owned);
        assert_eq!(back.summary().managed_by.as_deref(), Some("github"));
        assert_eq!(FleetRecord::with_owner(spec(), None), FleetRecord::new(spec()));
    }
```

In `crates/balerix-api/src/request.rs`, replace the expected string in `down_query_defaults_to_false_and_renders_every_flag` and add a case:

```rust
        assert_eq!(
            DownQuery {
                keep_repos: true,
                keep_sessions: false,
                purge: false,
                force: false,
            }
            .to_query_string(),
            "keep_repos=true&keep_sessions=false&purge=false&force=false"
        );
        // an older CLI sends no `force`: it is not a forced down
        let q: DownQuery = serde_json::from_value(serde_json::json!({
            "keep_repos": false, "keep_sessions": false, "purge": true
        }))
        .unwrap();
        assert!(!q.force);
        let q: DownQuery = serde_json::from_value(serde_json::json!({ "force": true })).unwrap();
        assert!(q.force && !q.purge);
```

In `crates/balerix-api/src/settings.rs` `mod tests`:

```rust
    /// Spec L §6: an optional existing remote branch. Absent and `null`
    /// are both "no branch"; the wire form omits it when unset.
    #[test]
    fn branch_is_optional_and_omitted_when_unset() {
        let s = AgentSettings::default();
        assert_eq!(s.branch, None);
        assert!(serde_json::to_value(&s).unwrap().get("branch").is_none());
        let s: AgentSettings =
            serde_json::from_value(json!({ "branch": "feature/issue-12" })).unwrap();
        assert_eq!(s.branch.as_deref(), Some("feature/issue-12"));
        assert_eq!(serde_json::to_value(&s).unwrap()["branch"], "feature/issue-12");
        let s: AgentSettings = serde_json::from_value(json!({ "branch": null })).unwrap();
        assert_eq!(s.branch, None);
        assert!(serde_json::from_value::<AgentSettings>(json!({ "branch": 3 })).is_err());
    }
```

In `crates/balerix-api/src/status.rs`, the `FleetSummary` literal in `down_is_a_phase_and_summaries_round_trip` gains `managed_by: None,` and the test gains:

```rust
        let older: FleetSummary = serde_json::from_value(json!({
            "name": "p", "phase": "ready", "generation": 1, "observed_generation": 1, "agents": 0
        }))
        .unwrap();
        assert_eq!(older.managed_by, None);
        let v = serde_json::to_value(&older).unwrap();
        assert!(v.get("managed_by").is_none(), "omitted when unowned");
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-api`
Expected: compile errors — no variant `Manage`, no field `owner`, `force`, `branch`, `managed_by`, `downed`.

- [ ] **Step 3: Implement the type changes**

`crates/balerix-api/src/plugin.rs`, the enum:

```rust
/// Host capabilities a plugin may declare (plugins spec §4.1; `manage` is
/// Spec L-1: apply and down a fleet from an unresolved fleet file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    Fleets,
    Actions,
    Attach,
    Kv,
    Workspace,
    Manage,
}
```

`SyncReport`:

```rust
/// What `POST /v1/plugins/sync` did, by plugin name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncReport {
    pub installed: Vec<String>,
    pub stopped: Vec<String>,
    pub unchanged: Vec<String>,
    /// Fleets downed because the plugin that owned them was stopped
    /// (Spec L-6). Absent from a report by an older daemon.
    #[serde(default)]
    pub downed: Vec<String>,
    /// `<fleet>: <error>` for each owned fleet the daemon could not down;
    /// the removal went on regardless.
    #[serde(default)]
    pub down_failed: Vec<String>,
}
```

`crates/balerix-api/src/record.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetRecord {
    pub spec: FleetSpec,
    /// The plugin that applied this fleet through `PUT
    /// /v1/plugin-host/fleets/{name}` (Spec L §5); `None` for a fleet the
    /// CLI created. Kept through `down`, so the plugin's next apply
    /// resumes it. Absent on the wire and in an older `fleet.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub generation: u64,
    pub desired: Desired,
    /// Agents held stopped by a plugin action (plugins spec §16.4): stopped
    /// if observed, never restarted, restart counter untouched. Keyed like
    /// `status.agents`. Cleared for every agent an `Apply` declares.
    #[serde(default)]
    pub stopped: BTreeSet<String>,
    pub status: FleetStatus,
}
```

```rust
impl FleetRecord {
    pub fn new(spec: FleetSpec) -> Self {
        Self::with_owner(spec, None)
    }
    /// A fresh record at generation 0, owned by `owner` when a plugin
    /// applied it.
    pub fn with_owner(spec: FleetSpec, owner: Option<String>) -> Self {
        Self {
            spec,
            owner,
            generation: 0,
            desired: Desired::Up,
            stopped: BTreeSet::new(),
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
            managed_by: self.owner.clone(),
        }
    }
}
```

`crates/balerix-api/src/status.rs`:

```rust
/// One row of `GET /v1/fleets`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetSummary {
    pub name: String,
    pub phase: FleetPhase,
    pub generation: u64,
    pub observed_generation: u64,
    pub agents: usize,
    /// `FleetRecord::owner` (Spec L §5): the plugin that manages the fleet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_by: Option<String>,
}
```

`crates/balerix-api/src/request.rs`:

```rust
/// Query flags of `DELETE /v1/fleets/{name}` (spec D6). Every flag is sent
/// as `key=true|false`: axum's `Query` rejects a bare key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DownQuery {
    pub keep_repos: bool,
    pub keep_sessions: bool,
    pub purge: bool,
    /// Down a plugin-managed fleet from the admin API (Spec L §5:
    /// `balerix down --force`). Ignored on the plugin-host route, where
    /// ownership is the rule.
    pub force: bool,
}

impl DownQuery {
    pub fn to_query_string(&self) -> String {
        format!(
            "keep_repos={}&keep_sessions={}&purge={}&force={}",
            self.keep_repos, self.keep_sessions, self.purge, self.force
        )
    }
}
```

`crates/balerix-api/src/settings.rs`: add the field after `plugins` and to `Default`:

```rust
    /// An existing remote branch this agent works on (Spec L §6). When
    /// set, the worktree branch is this name, created from
    /// `origin/<branch>`; the crew `ref` remains the base the workspace
    /// diff is taken against. Absent: the per-agent branch
    /// `balerix/<fleet>/<crew>/<agent>` from `origin/<ref>`. Validated by
    /// `check_branch_name` in the resolver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
```

```rust
            plugins: BTreeMap::new(),
            branch: None,
```

`crates/balerix-api/src/lib.rs`: add `pub mod branch;` and `pub use branch::check_branch_name;`; and in `pub use plugin::{…}` nothing changes (the variant is on the enum).

Compile fixes: in `crates/balerix/src/commands/plugin.rs` `renders_the_plugin_table_and_sync_report`, the `SyncReport { installed: …, stopped: vec![], unchanged: … }` literal gains `..SyncReport::default()`; in `crates/balerix/src/commands/fleet.rs` `down_command`, the `DownQuery { … }` literal gains `force: false,` (Task 8 wires the flag). Then grep for any other literal: `grep -rn "SyncReport {\|DownQuery {\|FleetSummary {" crates plugins --include=*.rs` and add the field to each.

- [ ] **Step 4: Write the failing test for `check_branch_name`**

Create `crates/balerix-api/src/branch.rs`:

```rust
//! The rule for an agent's `branch` (Spec L §6): what `git check-ref-format
//! --branch` accepts, as a pure function with a reason. It lives in this
//! leaf crate so `balerix-config` validates a fleet file with it and a
//! plugin that builds a fleet file can check a name before sending it.
//! The leading-`-` refusal is what keeps a name from being read as a flag
//! when it reaches `git worktree add -b <branch>` (Spec L §7).

/// `Err` is the reason, for `crews.<c>.agents.<a>.branch: <reason>`.
pub fn check_branch_name(name: &str) -> Result<(), String> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_what_git_check_ref_format_branch_accepts() {
        for ok in [
            "main",
            "feature/x",
            "release-1.2",
            "a.b/c-d_e",
            "pr/12/head",
            "x@y",
            "issue#12",
            "ünïcode",
            "a/b.lockfile",
            "v1.0",
        ] {
            assert_eq!(check_branch_name(ok), Ok(()), "{ok:?}");
        }
    }

    #[test]
    fn refuses_every_git_rule_with_a_reason() {
        for (bad, reason) in [
            ("", "empty"),
            ("-x", "starts with '-'"),
            ("--force", "starts with '-'"),
            ("refs/heads/main", "starts with \"refs/\""),
            ("/main", "starts with '/'"),
            ("main/", "ends with '/'"),
            ("main.", "ends with '.'"),
            ("a//b", "contains \"//\""),
            ("a..b", "contains \"..\""),
            ("a@{1}", "contains \"@{\""),
            ("@", "is \"@\""),
            ("a b", "contains a space"),
            ("a\tb", "contains a control character"),
            ("a\x7fb", "contains a control character"),
            ("a~1", "contains '~'"),
            ("a^b", "contains '^'"),
            ("a:b", "contains ':'"),
            ("a?b", "contains '?'"),
            ("a*b", "contains '*'"),
            ("a[b", "contains '['"),
            ("a\\b", "contains '\\\\'"),
            (".hidden", "a component starts with '.'"),
            ("a/.b", "a component starts with '.'"),
            ("a.lock", "a component ends with \".lock\""),
            ("a.lock/b", "a component ends with \".lock\""),
        ] {
            assert_eq!(check_branch_name(bad), Err(reason.to_string()), "{bad:?}");
        }
        let long = "x".repeat(256);
        assert_eq!(
            check_branch_name(&long),
            Err("longer than 255 bytes".to_string())
        );
        assert_eq!(check_branch_name(&"x".repeat(255)), Ok(()));
    }
}
```

- [ ] **Step 5: Run it to verify it fails**

Run: `mise x -- cargo nextest run -p balerix-api branch`
Expected: FAIL — `todo!()` panics.

- [ ] **Step 6: Implement `check_branch_name`**

```rust
pub fn check_branch_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("empty".into());
    }
    if name.len() > 255 {
        return Err("longer than 255 bytes".into());
    }
    if name == "@" {
        return Err("is \"@\"".into());
    }
    if name.starts_with('-') {
        return Err("starts with '-'".into());
    }
    if name.starts_with("refs/") {
        return Err("starts with \"refs/\"".into());
    }
    if name.starts_with('/') {
        return Err("starts with '/'".into());
    }
    if name.ends_with('/') {
        return Err("ends with '/'".into());
    }
    if name.ends_with('.') {
        return Err("ends with '.'".into());
    }
    if name.contains("//") {
        return Err("contains \"//\"".into());
    }
    if name.contains("..") {
        return Err("contains \"..\"".into());
    }
    if name.contains("@{") {
        return Err("contains \"@{\"".into());
    }
    for c in name.chars() {
        if c.is_ascii_control() {
            return Err("contains a control character".into());
        }
        if c == ' ' {
            return Err("contains a space".into());
        }
        if matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\') {
            return Err(format!("contains {c:?}"));
        }
    }
    for component in name.split('/') {
        if component.starts_with('.') {
            return Err("a component starts with '.'".into());
        }
        if component.ends_with(".lock") {
            return Err("a component ends with \".lock\"".into());
        }
    }
    Ok(())
}
```

(`format!("{c:?}")` renders `'\\'` for a backslash, which is what the table expects.)

- [ ] **Step 7: Run the whole gate**

Run: `mise run check`
Expected: PASS. If `clippy` flags the long `if` chain, keep it: each rule maps to one line of `git check-ref-format(1)`.

- [ ] **Step 8: Commit**

```bash
git add crates/balerix-api crates/balerix/src/commands/plugin.rs crates/balerix/src/commands/fleet.rs
git commit -m "feat(api): manage capability, fleet owner, down force, agent branch and its rule (Spec L §3, §5, §6)"
```

---

### Task 2: The resolver side (`balerix-config`)

**Files:**
- Modify: `crates/balerix-config/Cargo.toml`, `crates/balerix-config/src/file.rs`, `crates/balerix-config/src/validate.rs:34-95`, `crates/balerix-config/src/lib.rs`

**Interfaces:**
- Consumes: `balerix_api::check_branch_name`, `AgentSettings.branch` (Task 1).
- Produces: `FleetFile: Serialize`, `CrewFile: Serialize`; `pub fn from_value(value: &serde_json::Value) -> Result<FleetFile, ConfigError>` (re-exported at the crate root beside `parse`/`read`); `validate_agent` refusing a bad `branch` as `<path>.branch: <reason>`.

- [ ] **Step 1: Write the failing tests**

In `crates/balerix-config/src/validate.rs` `mod tests`:

```rust
    /// Spec L §6: a branch name is checked with git's own rule before it
    /// can reach a git argv; the error carries the agent's path.
    #[test]
    fn a_bad_branch_name_names_its_path_and_the_reason() {
        let s = AgentSettings {
            branch: Some("-x".into()),
            ..AgentSettings::default()
        };
        assert_eq!(
            validate_agent("crews.c.agents.a", &s).unwrap_err().to_string(),
            "crews.c.agents.a.branch: starts with '-'"
        );
        let s = AgentSettings {
            branch: Some("feature/issue-12".into()),
            ..AgentSettings::default()
        };
        validate_agent("crews.c.agents.a", &s).unwrap();
    }
```

In `crates/balerix-config/src/file.rs` `mod tests`:

```rust
    /// Spec L §3.1: the file crosses the plugin → daemon boundary as JSON;
    /// `from_value` is `parse` for that shape, with the same header checks.
    #[test]
    fn a_file_round_trips_through_json_and_from_value_checks_the_header() {
        let yaml = "apiVersion: balerix/v1\nkind: Fleet\nname: payments\ndefaults:\n  tools: { node: \"22.11.0\" }\ncrews:\n  backend:\n    repo: acme/api\n    ref: develop\n    git: { push: false, auth: none }\n    agents:\n      alice: { branch: feature/x }\n";
        let parsed = parse(yaml).unwrap();
        let v = serde_json::to_value(&parsed).unwrap();
        assert_eq!(v["crews"]["backend"]["ref"], "develop");
        assert_eq!(v["crews"]["backend"]["agents"]["alice"]["branch"], "feature/x");
        assert_eq!(from_value(&v).unwrap(), parsed);
        let mut no_name = v.clone();
        no_name.as_object_mut().unwrap().remove("name");
        let nameless = from_value(&no_name).unwrap();
        assert_eq!(nameless.name, None);
        assert!(
            serde_json::to_value(&nameless).unwrap().get("name").is_none(),
            "an absent name is not serialized as null"
        );
        let mut bad_kind = v.clone();
        bad_kind["kind"] = json!("Crew");
        assert_eq!(
            from_value(&bad_kind).unwrap_err().to_string(),
            "kind: expected \"Fleet\", got \"Crew\""
        );
        let mut bad_version = v;
        bad_version["apiVersion"] = json!("balerix/v2");
        assert_eq!(
            from_value(&bad_version).unwrap_err().to_string(),
            "apiVersion: expected \"balerix/v1\", got \"balerix/v2\""
        );
    }

    #[test]
    fn from_value_names_the_offending_key() {
        let e = from_value(&json!({
            "apiVersion": "balerix/v1", "kind": "Fleet",
            "crews": { "c": { "repo": "o/r", "nope": 1 } }
        }))
        .unwrap_err()
        .to_string();
        assert!(e.starts_with("crews.c: unknown field `nope`"), "{e}");
        let e = from_value(&json!({ "apiVersion": "balerix/v1", "kind": "Fleet", "extra": 1 }))
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("file: unknown field `extra`"), "{e}");
        let e = from_value(&json!([1])).unwrap_err().to_string();
        assert!(e.starts_with("file: "), "{e}");
        let e = from_value(&json!({ "apiVersion": "balerix/v1", "kind": "Fleet", "crews": { "c": {} } }))
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("crews.c: missing field `repo`"), "{e}");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-config`
Expected: compile errors — `FleetFile` is not `Serialize`, no `from_value`; then the branch test fails with `Ok(())`.

- [ ] **Step 3: Add the dependency and implement**

Root `Cargo.toml`, `[workspace.dependencies]`, after `serde_json`: `serde_path_to_error = "0.1.20"` (the pin `plugins/common/Cargo.toml` line 28 already uses; the root table has no entry yet). `crates/balerix-config/Cargo.toml` `[dependencies]` gains `serde_path_to_error = { workspace = true }`. Run `scripts/check-core-deps.sh` afterwards: it must still pass.

`crates/balerix-config/src/file.rs`:

```rust
use serde::{Deserialize, Serialize};
```

```rust
/// A parsed but unresolved fleet file. Settings layers are raw values.
/// `Serialize` so the file round-trips as the JSON object a plugin sends
/// the daemon (Spec L §3.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetFile {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Fleet-level settings layer.
    #[serde(default = "empty_object")]
    pub defaults: Value,
    #[serde(default)]
    pub crews: BTreeMap<String, CrewFile>,
}

/// One crew as written in the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    check_header(&file)?;
    Ok(file)
}

/// The same file as the JSON object a plugin sends the daemon (Spec L
/// §3.1). A shape error names the offending key's path, `file` for the
/// root, so the plugin author sees `crews.repo: missing field `repo``
/// rather than a bare serde message.
pub fn from_value(value: &Value) -> Result<FleetFile, ConfigError> {
    let file: FleetFile = serde_path_to_error::deserialize(value).map_err(|e| {
        let path = e.path().to_string();
        ConfigError::Invalid {
            path: if path == "." { "file".to_string() } else { path },
            message: e.inner().to_string(),
        }
    })?;
    check_header(&file)?;
    Ok(file)
}

fn check_header(file: &FleetFile) -> Result<(), ConfigError> {
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
    Ok(())
}
```

`crates/balerix-config/src/lib.rs`: `pub use file::{CrewFile, FleetFile, from_value, parse, read};`.

`crates/balerix-config/src/validate.rs`, after the `plugins` loop in `validate_agent`:

```rust
    // Spec L §6: the name reaches `git worktree add -b <branch>` as an argv
    // word; git's own rule, checked here, is what keeps it a name.
    if let Some(branch) = &settings.branch
        && let Err(reason) = balerix_api::check_branch_name(branch)
    {
        return Err(invalid("branch", reason));
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-config`
Expected: PASS. If the `serde_path_to_error` path for the top-level unknown field is not `.`, print it in the failing assertion, match on it in `from_value` (an empty string or `.` both map to `file`), and re-run.

- [ ] **Step 5: Run the gate and commit**

Run: `mise run check`
Expected: PASS.

```bash
git add Cargo.toml Cargo.lock crates/balerix-config
git commit -m "feat(config): validate an agent branch and read a fleet file from JSON (Spec L §3.1, §6)

Adds serde_path_to_error to balerix-config: a fleet file a plugin sends as
JSON must report the offending key's path, and serde_json alone names none."
```

---

### Task 3: The ports and the resolved agent (`balerix-core`)

**Files:**
- Modify: `crates/balerix-core/src/ports.rs`, `crates/balerix-core/src/agent.rs:101-105`, `crates/balerix-core/src/fakes.rs`, `crates/balerix-core/src/lib.rs`

**Interfaces:**
- Produces:
  ```rust
  pub trait FleetResolver: Send + Sync {
      fn resolve(&self, file: &serde_json::Value, name: &FleetName) -> Result<FleetSpec, String>;
  }
  pub trait CredentialSource: Send + Sync {
      fn load(&self) -> Result<CredentialBundle, String>;
  }
  impl ResolvedAgent { pub fn branch(&self) -> String; pub fn start_ref(&self) -> &str; }
  pub struct fakes::FakeResolver;  // Default (no answer → Err), answering(spec), failing(msg), set(answer), calls() -> Vec<(String, Value)>
  pub struct fakes::FakeCredentials; // Default (empty bundle), new(bundle), failing(msg), set(answer), calls() -> usize
  ```
  Both re-exported from `balerix_core` (`FleetResolver`, `CredentialSource`).

- [ ] **Step 1: Write the failing tests**

In `crates/balerix-core/src/agent.rs` `mod tests`:

```rust
    /// Spec L §6: `branch` names an existing remote branch; it is both the
    /// worktree branch and the start point. Without it, the per-agent
    /// branch starts from the crew's ref.
    #[test]
    fn branch_and_start_ref_follow_the_setting() {
        let mut a = ResolvedAgent::from_fleet(&fleet()).remove(0);
        assert_eq!(a.branch(), "balerix/payments/backend/alice");
        assert_eq!(a.start_ref(), "main");
        let before = a.hash();
        a.settings.branch = Some("feature/issue-12".into());
        assert_eq!(a.branch(), "feature/issue-12");
        assert_eq!(a.start_ref(), "feature/issue-12");
        assert_ne!(a.hash(), before, "a changed branch re-materializes the agent");
    }
```

In `crates/balerix-core/src/fakes.rs`, add at the end of the file (the crate has no `mod tests` in `fakes.rs`; add one):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::FleetSpec;
    use serde_json::json;

    #[test]
    fn the_fake_resolver_answers_under_the_requested_name_and_records_files() {
        let r = FakeResolver::default();
        let name: FleetName = "f".parse().unwrap();
        assert_eq!(
            r.resolve(&json!({}), &name),
            Err("name: no resolver answer configured".to_string())
        );
        r.set(Ok(FleetSpec {
            name: "other".into(),
            ..Default::default()
        }));
        let spec = r.resolve(&json!({ "kind": "Fleet" }), &name).unwrap();
        assert_eq!(spec.name, "f", "the fixed spec is renamed to the request");
        r.set(Err("crews.c.repo: invalid repo".into()));
        assert_eq!(
            r.resolve(&json!({}), &name),
            Err("crews.c.repo: invalid repo".to_string())
        );
        assert_eq!(
            r.calls(),
            vec![
                ("f".to_string(), json!({})),
                ("f".to_string(), json!({ "kind": "Fleet" })),
                ("f".to_string(), json!({})),
            ]
        );
        assert_eq!(FakeResolver::failing("x").resolve(&json!({}), &name), Err("x".to_string()));
        assert_eq!(
            FakeResolver::answering(FleetSpec::default())
                .resolve(&json!({}), &name)
                .unwrap()
                .name,
            "f"
        );
    }

    #[test]
    fn the_fake_credentials_answer_a_bundle_or_an_error_and_count_calls() {
        let c = FakeCredentials::default();
        assert_eq!(c.load(), Ok(CredentialBundle::default()));
        let bundle = CredentialBundle {
            gh_token: Some("gho_x".into()),
            ..CredentialBundle::default()
        };
        let c = FakeCredentials::new(bundle.clone());
        assert_eq!(c.load(), Ok(bundle));
        c.set(Err("/home/op/.claude/settings.json: invalid JSON".into()));
        assert!(c.load().is_err());
        assert_eq!(c.calls(), 2);
        assert!(FakeCredentials::failing("no").load().is_err());
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-core`
Expected: compile errors — no `start_ref`, `FakeResolver`, `FakeCredentials`.

- [ ] **Step 3: Implement**

`crates/balerix-core/src/ports.rs`, imports:

```rust
use balerix_api::{
    CredentialBundle, FleetSpec, GitSettings, Timestamp, WorkspaceDiff, WorkspaceTree,
    WorkspaceVersion,
};
```

After the `Clock` trait:

```rust
/// Resolves an unresolved fleet file (Spec L §4): the daemon's half of
/// what `balerix up` does client-side, so a plugin's file goes through the
/// one resolver. Implemented by the binary over `balerix-config`;
/// `balerix-server` never sees that crate. Sync like every port; the
/// daemon calls it in `spawn_blocking` (it reads the host's defaults).
pub trait FleetResolver: Send + Sync {
    /// `file` is the YAML fleet file's structure as JSON, `name` the fleet
    /// it must resolve to (a `name` inside the file has already been
    /// checked against it). The host's `claude.settings` are folded in.
    /// `Err` is the resolver's own message, config path first.
    fn resolve(&self, file: &serde_json::Value, name: &FleetName) -> Result<FleetSpec, String>;
}

/// The operator's Claude credentials and gh token, read from the host
/// home now, as `up` reads them (Spec L-3). A plugin never holds them:
/// the daemon reads the bundle at apply time.
pub trait CredentialSource: Send + Sync {
    fn load(&self) -> Result<CredentialBundle, String>;
}
```

`crates/balerix-core/src/agent.rs`:

```rust
    /// The worktree branch: `settings.branch` when set (an existing remote
    /// branch, Spec L §6), else the per-agent
    /// `balerix/<fleet>/<crew>/<agent>` (spec D3).
    pub fn branch(&self) -> String {
        match &self.settings.branch {
            Some(b) => b.clone(),
            None => format!("balerix/{}", self.id),
        }
    }

    /// The remote ref the worktree branch is created from when it does
    /// not exist locally: the branch itself when `settings.branch` is set,
    /// else the crew's `ref`. The workspace diff base is the crew's `ref`
    /// either way (`Daemon::base_ref`).
    pub fn start_ref(&self) -> &str {
        self.settings.branch.as_deref().unwrap_or(&self.git_ref)
    }
```

`crates/balerix-core/src/fakes.rs`: add to the imports `use balerix_api::{CredentialBundle, FleetSpec};`, `use serde_json::Value;`, `use std::sync::atomic::{AtomicUsize, Ordering};` and `crate::ports::{CredentialSource, FleetResolver}` (merge with the existing `use` lines; `lock` is the file's existing helper over a poisoned `Mutex`). Then:

```rust
/// `FleetResolver` for daemon tests: answers a fixed spec, renamed to the
/// requested fleet so one fake serves several names, or a fixed error;
/// records every `(fleet, file)` it was asked to resolve.
#[derive(Default)]
pub struct FakeResolver {
    answer: Mutex<Option<Result<FleetSpec, String>>>,
    calls: Mutex<Vec<(String, Value)>>,
}

impl FakeResolver {
    pub fn answering(spec: FleetSpec) -> Self {
        let r = Self::default();
        r.set(Ok(spec));
        r
    }
    pub fn failing(message: &str) -> Self {
        let r = Self::default();
        r.set(Err(message.to_string()));
        r
    }
    pub fn set(&self, answer: Result<FleetSpec, String>) {
        *lock(&self.answer) = Some(answer);
    }
    /// `(fleet name, file)` per call, in order.
    pub fn calls(&self) -> Vec<(String, Value)> {
        lock(&self.calls).clone()
    }
}

impl FleetResolver for FakeResolver {
    fn resolve(&self, file: &Value, name: &FleetName) -> Result<FleetSpec, String> {
        lock(&self.calls).push((name.to_string(), file.clone()));
        match lock(&self.answer).clone() {
            Some(Ok(mut spec)) => {
                spec.name = name.to_string();
                Ok(spec)
            }
            Some(Err(e)) => Err(e),
            None => Err("name: no resolver answer configured".to_string()),
        }
    }
}

/// `CredentialSource` for daemon tests: a fixed bundle (empty by default)
/// or a fixed error, and a count of loads.
pub struct FakeCredentials {
    answer: Mutex<Result<CredentialBundle, String>>,
    calls: AtomicUsize,
}

impl Default for FakeCredentials {
    fn default() -> Self {
        Self::new(CredentialBundle::default())
    }
}

impl FakeCredentials {
    pub fn new(bundle: CredentialBundle) -> Self {
        Self {
            answer: Mutex::new(Ok(bundle)),
            calls: AtomicUsize::new(0),
        }
    }
    pub fn failing(message: &str) -> Self {
        let c = Self::default();
        c.set(Err(message.to_string()));
        c
    }
    pub fn set(&self, answer: Result<CredentialBundle, String>) {
        *lock(&self.answer) = answer;
    }
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl CredentialSource for FakeCredentials {
    fn load(&self) -> Result<CredentialBundle, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        lock(&self.answer).clone()
    }
}
```

`crates/balerix-core/src/lib.rs`, the `ports` re-export:

```rust
pub use ports::{
    AgentRunner, Clock, CredentialSource, CrewTools, FleetResolver, HookTarget, Keep, LaunchPlan,
    MaterializeError, Materializer, ObservedState, ProcessState, PtyStream, RunnerError,
    SystemToolchain, WorkspaceError, WorkspaceReader, first_line,
};
```

- [ ] **Step 4: Run the tests, then the gate**

Run: `mise x -- cargo nextest run -p balerix-core` then `mise run check`
Expected: PASS. (`.cargo/mutants.toml` excludes `fakes.rs`; nothing to add there.)

- [ ] **Step 5: Commit**

```bash
git add crates/balerix-core
git commit -m "feat(core): FleetResolver and CredentialSource ports; the agent branch and its start ref (Spec L §4, §6)"
```

---

### Task 4: The worktree on an existing remote branch (`balerix-runtime`)

**Files:**
- Modify: `crates/balerix-runtime/src/materializer.rs:335-350`, `crates/balerix-runtime/tests/workspace_it.rs`

**Interfaces:**
- Consumes: `ResolvedAgent::{branch, start_ref}` (Task 3); `Workspace::ensure_worktree(id, crew, workspace, branch, git_ref)` (unchanged).

- [ ] **Step 1: Write the failing integration tests**

Append to `crates/balerix-runtime/tests/workspace_it.rs`:

```rust
/// Pushes a second commit on `branch` to the bare repo `bare_repo` made,
/// through the upstream working copy.
fn push_branch(root: &Path, branch: &str) -> String {
    let work = root.join("upstream-work");
    git(&work, &["checkout", "-q", "-b", branch]);
    std::fs::write(work.join("FEATURE"), "wip\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "feature work"]);
    let bare = root.join("upstream.git").display().to_string();
    git(&work, &["push", "-q", &bare, branch]);
    git(&work, &["checkout", "-q", "main"]);
    git(&work, &["rev-parse", branch]).trim().to_string()
}

/// Spec L §6: an agent with `branch` works on that remote branch — created
/// from `origin/<branch>`, tracking it, reused across passes — never on a
/// fresh `balerix/…` branch.
#[test]
fn a_worktree_on_an_existing_remote_branch_is_created_from_it_and_reused() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-branch");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let tip = push_branch(&root, "feature/issue-12");
    let id: balerix_core::AgentId = "f/c/pr".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();

    ws.ensure_worktree(
        "f/c/pr",
        &crew,
        &paths.workspace,
        "feature/issue-12",
        "feature/issue-12",
    )
    .unwrap();
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "feature/issue-12"
    );
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]).trim(), tip);
    assert!(paths.workspace.join("FEATURE").exists());
    assert_eq!(
        git(
            &paths.workspace,
            &["rev-parse", "--abbrev-ref", "feature/issue-12@{upstream}"]
        )
        .trim(),
        "origin/feature/issue-12",
        "a push from the worktree reaches the PR's branch"
    );

    // the agent commits; a removed and re-added worktree keeps the branch
    std::fs::write(paths.workspace.join("more"), "x\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);
    ws.remove_worktree("f/c/pr", &crew, &paths.workspace).unwrap();
    ws.ensure_worktree(
        "f/c/pr",
        &crew,
        &paths.workspace,
        "feature/issue-12",
        "feature/issue-12",
    )
    .unwrap();
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]), sha);
}

/// Review focus 1: a `branch` the remote does not have fails the
/// materialize step with git's message; nothing is created from `main`.
#[test]
fn a_missing_remote_branch_fails_the_worktree() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-nobranch");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/pr".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    let e = ws
        .ensure_worktree("f/c/pr", &crew, &paths.workspace, "nope", "nope")
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("f/c/pr: git worktree:"), "{e}");
    assert!(e.contains("origin/nope"), "{e}");
    assert!(!paths.workspace.join(".git").exists());
    assert!(
        git(&crew.repo, &["branch", "--list", "nope"]).trim().is_empty(),
        "no local branch was created"
    );
}

/// `check_branch_name` is a model of `git check-ref-format --branch`;
/// this is the only place the model meets the real thing.
#[test]
fn check_branch_name_agrees_with_git_check_ref_format() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    for name in [
        "main", "feature/x", "release-1.2", "a.b/c-d_e", "pr/12/head", "x@y", "issue#12",
        "a/b.lockfile", "v1.0", "", "refs/heads/main", "/main", "main/", "main.", "a//b",
        "a..b", "a@{1}", "@", "a b", "a~1", "a^b", "a:b", "a?b", "a*b", "a[b", "a\\b",
        ".hidden", "a/.b", "a.lock", "a.lock/b",
    ] {
        let git_ok = Command::new(&tools.git)
            .args(["check-ref-format", "--branch", name])
            .output()
            .unwrap()
            .status
            .success();
        assert_eq!(
            balerix_api::check_branch_name(name).is_ok(),
            git_ok,
            "{name:?}: git says {git_ok}"
        );
    }
}
```

(`balerix-api` is already a dependency of `balerix-runtime`; `support::tools()` gives `ToolPaths` with a `git` field.)

- [ ] **Step 2: Run them to verify the expected failures**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --test workspace_it`
Expected: `a_worktree_on_an_existing_remote_branch…` and `a_missing_remote_branch…` PASS already (the workspace layer takes the branch and the start point as arguments; this task's change is the caller); `check_branch_name_agrees…` PASS. If any of the three fails, the failure is real: fix `check_branch_name` (Task 1) or the assertion on git's wording, not the workspace code.

- [ ] **Step 3: Write the failing unit test for the materializer's call**

`Runtime::materialize` is exercised by `crates/balerix-runtime/tests/materialize_it.rs::materialize_then_remove_round_trip` against a bare repo it builds under `root.join("up")` / `root.join("up.git")` (lines 106-122). Add to that test, right after the first `rt.materialize(&agent, &creds, &hooks)` and its `README` assertion (line 147-149):

```rust
    // Spec L §6: an agent with `branch` works on that remote branch. Push
    // one to the bare repo through the working copy, then resolve a
    // second agent that names it.
    git(&work, &["checkout", "-q", "-b", "feature/issue-12"]);
    std::fs::write(work.join("FEATURE"), "wip\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "feature work"]);
    git(&work, &["push", "-q", &bare.display().to_string(), "feature/issue-12"]);
    let mut pr = agent.clone();
    pr.id = "f/c/pr".parse().unwrap();
    pr.settings.branch = Some("feature/issue-12".into());
    rt.materialize(&pr, &creds, &hooks).unwrap();
    let pr_paths = layout.agent(&pr.id);
    assert_eq!(
        git(&pr_paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "feature/issue-12",
        "the worktree is on the remote branch, not on balerix/f/c/pr"
    );
    assert!(pr_paths.workspace.join("FEATURE").exists());
    assert!(
        !paths.workspace.join("FEATURE").exists(),
        "the first agent's worktree is still on its own branch from main"
    );
```

(`git` in that file returns stdout as a `String`, like `workspace_it.rs`'s; `agent`, `bare`, `work`, `paths` and `layout` are the test's existing locals.)

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --test materialize_it`
Expected: FAIL — HEAD is `balerix/f/c/pr`.

- [ ] **Step 4: Change the materializer**

`crates/balerix-runtime/src/materializer.rs`, in `Materializer::materialize`:

```rust
        // Spec L §6: with `branch` set, the worktree branch is the remote
        // branch itself and is created from `origin/<branch>`; without it
        // the per-agent branch starts from the crew's ref.
        self.workspace(&agent.id.fleet, &agent.git)
            .ensure_worktree(
                &id,
                &crew,
                &paths.workspace,
                &agent.branch(),
                agent.start_ref(),
            )?;
```

- [ ] **Step 5: Run the integration tier and the gate**

Run: `mise run test-it` then `mise run check`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/balerix-runtime
git commit -m "feat(runtime): create an agent's worktree from its own remote branch when one is set (Spec L §6)"
```

---

### Task 5: Owned fleets, the caller rule and `manage_fleet` (`balerix-server` + binary wiring)

**Files:**
- Modify: `crates/balerix-server/src/actor.rs:64-77` (`Ports`); `crates/balerix-server/src/daemon.rs` (`DaemonError`, `apply`, `down`, `sync_plugins`, new `Caller`, `ApplyMode`, `manage_fleet`, tests); `crates/balerix-server/src/api.rs:67-78` (the 409 mapping); `crates/balerix-server/src/plugins/host.rs:86-96` (the `Ports` copy); `crates/balerix-server/src/testing.rs` (`Harness`); `crates/balerix-server/src/lib.rs` (re-exports); `crates/balerix-server/tests/plugins_it.rs:396` (a `Ports` literal)
- Modify: `crates/balerix/src/wiring.rs` (`HostResolver`), `crates/balerix/src/commands/serve.rs:161-171`

**Interfaces:**
- Consumes: `FleetResolver`, `CredentialSource`, `FakeResolver`, `FakeCredentials` (Task 3); `FleetRecord::with_owner`, `.owner` (Task 1); `balerix_config::from_value` (Task 2).
- Produces:
  ```rust
  pub struct Ports { …, pub resolver: Arc<dyn FleetResolver>, pub credentials: Arc<dyn CredentialSource>, … }
  pub enum Caller { Admin { force: bool }, Plugin(AgentName) }
  pub enum ApplyMode { Create, Replace, Upsert }
  pub enum DaemonError { …, Managed(String) }   // → 409
  impl Daemon {
      pub async fn apply(&self, name, spec, credentials, replace: bool) -> Result<FleetRecord, DaemonError>;      // unchanged: Admin, Create/Replace
      pub async fn apply_as(&self, name, spec, credentials, mode: ApplyMode, caller: &Caller) -> Result<FleetRecord, DaemonError>;
      pub async fn down(&self, name, keep, purge) -> Result<FleetRecord, DaemonError>;                            // unchanged: Admin { force: false }
      pub async fn down_as(&self, name, keep, purge, caller: &Caller) -> Result<FleetRecord, DaemonError>;
      pub async fn manage_fleet(&self, plugin: &AgentName, name: &FleetName, file: serde_json::Value) -> Result<FleetRecord, DaemonError>;
  }
  // Harness gains `pub resolver: Arc<FakeResolver>`, `pub credentials: Arc<FakeCredentials>`.
  // balerix::wiring::HostResolver implements both ports; `wiring::resolve_file(file, name, defaults)` is its pure half.
  ```

- [ ] **Step 1: Write the failing daemon tests**

In `crates/balerix-server/src/daemon.rs` `mod tests`, add these helpers after `wait_gen`:

```rust
    async fn wait_down(daemon: &Daemon) {
        let name: FleetName = "f".parse().unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if daemon.get(&name).await.is_some_and(|r| r.is_down()) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    /// The Spec L §3.1 path, as the route calls it, for fleet `f`.
    async fn manage(
        w: &World,
        plugin: &str,
        file: serde_json::Value,
    ) -> Result<FleetRecord, DaemonError> {
        let name: FleetName = "f".parse().unwrap();
        w.daemon
            .manage_fleet(&plugin.parse().unwrap(), &name, file)
            .await
    }

    /// An unresolved fleet file; what the fake resolver answers is set by
    /// each test, so the crews here are only what the name check reads.
    fn file() -> serde_json::Value {
        json!({ "apiVersion": "balerix/v1", "kind": "Fleet", "name": "f", "crews": {} })
    }
```

and these tests (add `Desired` and `Keep` to the test module's `use` if they are not visible; both are imported at the top of the file):

```rust
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_plugin_applies_a_fleet_from_a_file_and_owns_it() {
        let w = world().await;
        let name: FleetName = "f".parse().unwrap();
        w.h.resolver.set(Ok(spec(&[("a", &[])])));
        let rec = manage(&w, "flow", file()).await.unwrap();
        assert_eq!(rec.owner.as_deref(), Some("flow"));
        assert_eq!(rec.generation, 1);
        assert_eq!(rec.spec.name, "f");
        assert_eq!(w.h.resolver.calls(), vec![("f".to_string(), file())]);
        assert_eq!(
            w.h.credentials.calls(),
            1,
            "the operator's bundle is read at apply time"
        );
        wait_gen(&w.daemon, 1).await;
        assert!(
            w.h.materializer.calls().iter().any(|c| c.contains("f/c/a")),
            "the fleet runs: {:?}",
            w.h.materializer.calls()
        );

        // a second apply is an upsert: replaced in place
        let rec = manage(&w, "flow", file()).await.unwrap();
        assert_eq!((rec.generation, rec.owner.as_deref()), (2, Some("flow")));

        // the admin routes refuse it, and so does another plugin
        let managed = DaemonError::Managed("fleet f is managed by plugin flow".into());
        assert_eq!(
            w.daemon
                .apply(&name, spec(&[("a", &[])]), Default::default(), false)
                .await
                .unwrap_err(),
            managed
        );
        assert_eq!(
            w.daemon
                .apply(&name, spec(&[("a", &[])]), Default::default(), true)
                .await
                .unwrap_err(),
            managed
        );
        assert_eq!(
            w.daemon
                .down(&name, Keep::default(), false)
                .await
                .unwrap_err(),
            managed
        );
        assert_eq!(manage(&w, "web", file()).await.unwrap_err(), managed);
        let web = Caller::Plugin("web".parse().unwrap());
        assert_eq!(
            w.daemon
                .down_as(&name, Keep::default(), false, &web)
                .await
                .unwrap_err(),
            managed
        );
        assert_eq!(
            w.h.resolver.calls().len(),
            2,
            "a foreign plugin's apply is refused before resolving"
        );

        // the owner downs it; the owner stays; the owner's next apply resumes it
        let flow = Caller::Plugin("flow".parse().unwrap());
        let rec = w
            .daemon
            .down_as(&name, Keep::default(), false, &flow)
            .await
            .unwrap();
        assert!(matches!(rec.desired, Desired::Down { .. }));
        assert_eq!(rec.owner.as_deref(), Some("flow"));
        assert_eq!(w.daemon.list().await[0].managed_by.as_deref(), Some("flow"));
        wait_down(&w.daemon).await;
        let rec = manage(&w, "flow", file()).await.unwrap();
        assert_eq!((rec.generation, rec.desired), (3, Desired::Up));
        // and the admin can force it down
        let rec = w
            .daemon
            .down_as(&name, Keep::default(), false, &Caller::Admin { force: true })
            .await
            .unwrap();
        assert!(matches!(rec.desired, Desired::Down { .. }));
        assert_eq!(rec.owner.as_deref(), Some("flow"), "a forced down keeps the owner");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_plugin_cannot_take_a_fleet_the_cli_created() {
        let w = world().await;
        let name: FleetName = "f".parse().unwrap();
        w.daemon
            .apply(&name, spec(&[("a", &[])]), Default::default(), false)
            .await
            .unwrap();
        w.h.resolver.set(Ok(spec(&[("a", &[])])));
        let not_managed = DaemonError::Managed("fleet f is not managed by a plugin".into());
        assert_eq!(manage(&w, "flow", file()).await.unwrap_err(), not_managed);
        let flow = Caller::Plugin("flow".parse().unwrap());
        assert_eq!(
            w.daemon
                .down_as(&name, Keep::default(), false, &flow)
                .await
                .unwrap_err(),
            not_managed
        );
        assert!(w.h.resolver.calls().is_empty(), "refused before resolving");
        // even once it is down: ownership is never transferred (Spec L §9)
        w.daemon.down(&name, Keep::default(), false).await.unwrap();
        wait_down(&w.daemon).await;
        assert_eq!(manage(&w, "flow", file()).await.unwrap_err(), not_managed);
        // while the CLI still may re-apply it in place
        assert_eq!(
            w.daemon
                .apply(&name, spec(&[("a", &[])]), Default::default(), false)
                .await
                .unwrap()
                .generation,
            2
        );
    }

    /// Review focus 2.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_file_whose_name_disagrees_with_the_path_is_refused_before_resolving() {
        let w = world().await;
        let mut f = file();
        f["name"] = json!("g");
        assert_eq!(
            manage(&w, "flow", f).await.unwrap_err(),
            DaemonError::Invalid("name: \"g\" does not match the fleet f".into())
        );
        assert!(w.h.resolver.calls().is_empty());
        assert_eq!(w.h.credentials.calls(), 0);
        // a file without a name resolves under the path's name
        let mut f = file();
        f.as_object_mut().unwrap().remove("name");
        w.h.resolver.set(Ok(spec(&[("a", &[])])));
        assert_eq!(manage(&w, "flow", f).await.unwrap().spec.name, "f");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_resolver_or_credential_failure_lands_nothing() {
        let w = world().await;
        let name: FleetName = "f".parse().unwrap();
        let bad_version = "crews.c.agents.a.tools.node: expected an exact version, got \"22\" (try: mise latest node@22)";
        w.h.resolver.set(Err(bad_version.into()));
        assert_eq!(
            manage(&w, "flow", file()).await.unwrap_err(),
            DaemonError::Invalid(bad_version.into())
        );
        assert!(w.daemon.get(&name).await.is_none(), "no actor was spawned");
        assert_eq!(
            w.h.credentials.calls(),
            0,
            "credentials are read only after a successful resolve"
        );
        w.h.resolver.set(Ok(spec(&[("a", &[])])));
        w.h.credentials
            .set(Err("/home/op/.claude/settings.json: invalid JSON".into()));
        assert_eq!(
            manage(&w, "flow", file()).await.unwrap_err(),
            DaemonError::Internal("/home/op/.claude/settings.json: invalid JSON".into())
        );
        assert!(w.daemon.get(&name).await.is_none());
        // a rejected activation is still a 400 with nothing landed: the
        // apply's existing rule holds for a plugin's apply too
        let stub = stub_plugin(StubScript {
            reject: BTreeMap::from([("f/c/bad".to_string(), "no".to_string())]),
            health_ok: true,
            ..StubScript::default()
        })
        .await;
        hello(&w, &stub.listen).await;
        w.h.credentials.set(Ok(Default::default()));
        w.h.resolver
            .set(Ok(spec(&[("bad", &[("flow", json!({}))])])));
        assert_eq!(
            manage(&w, "flow", file()).await.unwrap_err(),
            DaemonError::Invalid("crews.c.agents.bad.plugins.flow: no".into())
        );
        assert!(w.daemon.get(&name).await.is_none());
    }

    /// Review focus 5.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reserved_names_are_refused_by_the_manage_path() {
        let w = world().await;
        w.h.resolver.set(Ok(spec(&[("a", &[])])));
        for reserved in ["balerix", "watch"] {
            let name: FleetName = reserved.parse().unwrap();
            let e = w
                .daemon
                .manage_fleet(
                    &"flow".parse().unwrap(),
                    &name,
                    json!({ "apiVersion": "balerix/v1", "kind": "Fleet", "crews": {} }),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(&e, DaemonError::Invalid(m) if m.starts_with("name:")),
                "{reserved}: {e}"
            );
        }
        assert!(w.h.resolver.calls().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn removing_a_plugin_downs_the_fleets_it_owns() {
        let w = world().await;
        let flow: AgentName = "flow".parse().unwrap();
        w.h.resolver.set(Ok(spec(&[("a", &[])])));
        manage(&w, "flow", file()).await.unwrap();
        // beside it: a CLI fleet, and an owned fleet already going down
        let g: FleetName = "g".parse().unwrap();
        let mut g_spec = spec(&[("a", &[])]);
        g_spec.name = "g".into();
        w.daemon
            .apply(&g, g_spec, Default::default(), false)
            .await
            .unwrap();
        let h: FleetName = "h".parse().unwrap();
        let nameless = json!({ "apiVersion": "balerix/v1", "kind": "Fleet", "crews": {} });
        w.daemon.manage_fleet(&flow, &h, nameless).await.unwrap();
        w.daemon
            .down_as(&h, Keep::default(), false, &Caller::Plugin(flow.clone()))
            .await
            .unwrap();

        std::fs::write(w.dir.join("plugins.yaml"), "plugins: []\n").unwrap();
        let report = w.daemon.sync_plugins().await.unwrap();
        assert_eq!(report.stopped, vec!["flow".to_string()]);
        assert_eq!(
            report.downed,
            vec!["f".to_string()],
            "only the owned fleet that was up"
        );
        assert!(report.down_failed.is_empty());
        let f = w.daemon.get(&"f".parse().unwrap()).await.unwrap();
        assert!(matches!(f.desired, Desired::Down { .. }));
        assert_eq!(
            f.owner.as_deref(),
            Some("flow"),
            "the owner is kept through the down"
        );
        assert_eq!(
            w.daemon.get(&g).await.unwrap().desired,
            Desired::Up,
            "the CLI's fleet is untouched"
        );
    }

    /// Review focus 4.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stored_owned_record_keeps_its_owner_after_a_restart() {
        let stored = FleetRecord::with_owner(spec(&[("a", &[])]), Some("flow".into()));
        let w = world_with(vec![(stored, FleetSecrets::default())]).await;
        let name: FleetName = "f".parse().unwrap();
        assert_eq!(
            w.daemon.get(&name).await.unwrap().owner.as_deref(),
            Some("flow")
        );
        assert_eq!(w.daemon.list().await[0].managed_by.as_deref(), Some("flow"));
        let managed = DaemonError::Managed("fleet f is managed by plugin flow".into());
        assert_eq!(
            w.daemon
                .apply(&name, spec(&[("a", &[])]), Default::default(), true)
                .await
                .unwrap_err(),
            managed
        );
        assert_eq!(
            w.daemon
                .down(&name, Keep::default(), false)
                .await
                .unwrap_err(),
            managed
        );
        // the plugin resumes it; the admin can force it down
        w.h.resolver.set(Ok(spec(&[("a", &[])])));
        assert_eq!(manage(&w, "flow", file()).await.unwrap().generation, 1);
        w.daemon
            .down_as(&name, Keep::default(), false, &Caller::Admin { force: true })
            .await
            .unwrap();
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-server daemon::`
Expected: compile errors — no `resolver`/`credentials` on `Harness`, no `manage_fleet`, `Caller`, `down_as`, `Managed`.

- [ ] **Step 3: Add the two ports to `Ports` and every place that builds one**

`crates/balerix-server/src/actor.rs`:

```rust
use balerix_core::{
    …, CredentialSource, FleetResolver, …   // add to the existing `use balerix_core::{…}`
};

/// Everything every actor shares read-only.
pub struct Ports {
    pub materializer: Arc<dyn Materializer>,
    pub runner: Arc<dyn AgentRunner>,
    pub clock: Arc<dyn Clock>,
    pub store: Arc<dyn FleetStore>,
    /// Read-only worktree access for the plugin host's workspace routes
    /// (Spec C §3.1). Not used by the reconciler.
    pub workspace: Arc<dyn WorkspaceReader>,
    /// Resolves a plugin's fleet file (Spec L §4). Not used by the
    /// reconciler.
    pub resolver: Arc<dyn FleetResolver>,
    /// The operator's credentials for a plugin-managed fleet (Spec L-3).
    /// Not used by the reconciler.
    pub credentials: Arc<dyn CredentialSource>,
    pub policy: ReconcilePolicy,
    /// `http://127.0.0.1:<port>`; every agent's hooks post here.
    pub hook_url: String,
    pub resync: Duration,
}
```

`crates/balerix-server/src/plugins/host.rs` `PluginHost::start`, the `Ports` copy gains:

```rust
            resolver: agent_ports.resolver.clone(),
            credentials: agent_ports.credentials.clone(),
```

`crates/balerix-server/src/testing.rs`: import `FakeCredentials, FakeResolver` from `balerix_core::fakes`; `Harness` gains

```rust
    pub resolver: Arc<FakeResolver>,
    pub credentials: Arc<FakeCredentials>,
```

`with_policy` creates `let resolver = Arc::new(FakeResolver::default()); let credentials = Arc::new(FakeCredentials::default());`, puts `resolver: resolver.clone(), credentials: credentials.clone(),` into the `Ports` and the two fields into `Self { … }`; `daemon_full`'s `Ports` gains `resolver: self.resolver.clone(), credentials: self.credentials.clone(),`.

`crates/balerix-server/tests/plugins_it.rs`, the `balerix_server::Ports { … }` literal gains `resolver: h.resolver.clone(), credentials: h.credentials.clone(),`.

`crates/balerix/src/wiring.rs`:

```rust
use balerix_api::{CredentialBundle, FleetSpec, Timestamp};
use balerix_config::{HostDefaults, HostPaths, ResolveOptions, from_value, host, resolve};
use balerix_core::{Clock, CredentialSource, Fleet, FleetName, FleetResolver};
use serde_json::Value;

/// The two Spec L ports over `balerix-config`: what
/// `commands::fleet::load_request` does for `up`, with the host's
/// defaults read at call time, so a daemon that outlives a `gh auth
/// login` hands the next managed fleet the new token.
pub struct HostResolver;

/// The pure half: `file` resolved as fleet `name` beneath `defaults`.
pub fn resolve_file(
    file: &Value,
    name: &FleetName,
    defaults: &HostDefaults,
) -> Result<FleetSpec, String> {
    let file = from_value(file).map_err(|e| e.to_string())?;
    let spec = resolve(
        &file,
        &ResolveOptions {
            name_override: Some(name.to_string()),
            host_claude_settings: defaults.claude_settings.clone(),
        },
    )
    .map_err(|e| e.to_string())?;
    // names and repos, as `load_request` checks before any request
    Fleet::try_from(spec.clone()).map_err(|e| e.to_string())?;
    Ok(spec)
}

fn host_defaults() -> Result<HostDefaults, String> {
    let paths = HostPaths::discover().map_err(|e| e.to_string())?;
    host::load(&paths).map_err(|e| e.to_string())
}

impl FleetResolver for HostResolver {
    fn resolve(&self, file: &Value, name: &FleetName) -> Result<FleetSpec, String> {
        resolve_file(file, name, &host_defaults()?)
    }
}

impl CredentialSource for HostResolver {
    fn load(&self) -> Result<CredentialBundle, String> {
        host_defaults().map(|d| d.credentials)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file() -> Value {
        json!({
            "apiVersion": "balerix/v1", "kind": "Fleet",
            "crews": { "c": { "repo": "acme/api", "agents": { "a": { "branch": "feature/x" } } } }
        })
    }

    #[test]
    fn resolve_file_folds_the_host_settings_in_and_keeps_the_branch() {
        let name: FleetName = "f".parse().unwrap();
        let defaults = HostDefaults {
            claude_settings: Some(json!({ "model": "haiku" })),
            ..HostDefaults::default()
        };
        let spec = resolve_file(&file(), &name, &defaults).unwrap();
        assert_eq!(spec.name, "f");
        let a = &spec.crews["c"].agents["a"];
        assert_eq!(a.branch.as_deref(), Some("feature/x"));
        assert_eq!(a.claude.settings["model"], "haiku");
    }

    #[test]
    fn resolve_file_errors_carry_the_config_path() {
        let name: FleetName = "f".parse().unwrap();
        let mut f = file();
        f["crews"]["c"]["agents"]["a"]["tools"] = json!({ "node": "22" });
        let e = resolve_file(&f, &name, &HostDefaults::default()).unwrap_err();
        assert!(e.starts_with("crews.c.agents.a.tools.node:"), "{e}");
        let e = resolve_file(&json!({ "kind": "Fleet" }), &name, &HostDefaults::default())
            .unwrap_err();
        assert!(e.starts_with("file: missing field `apiVersion`"), "{e}");
    }
}
```

`crates/balerix/src/commands/serve.rs`: import `crate::wiring::HostResolver` and add to the `Ports` literal

```rust
            resolver: Arc::new(HostResolver),
            credentials: Arc::new(HostResolver),
```

- [ ] **Step 4: Implement the caller rule, the modes and `manage_fleet`**

`crates/balerix-server/src/daemon.rs`. After `DaemonError`:

```rust
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
    /// The owner rule (Spec L §5): a 409 naming who manages the fleet.
    #[error("{0}")]
    Managed(String),
}

/// Who is applying or downing a fleet (Spec L §5). The admin API and a
/// plugin with `manage` share `apply_as`/`down_as`; the owner rule reads
/// the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// The admin API. `force` lets `down` take a plugin-managed fleet
    /// (`balerix down --force`); nothing lets the admin apply one.
    Admin { force: bool },
    /// A plugin with `manage`, by name.
    Plugin(AgentName),
}

impl Caller {
    fn owner(&self) -> Option<String> {
        match self {
            Caller::Admin { .. } => None,
            Caller::Plugin(p) => Some(p.to_string()),
        }
    }
}

/// How an apply treats the record it finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyMode {
    /// `POST /v1/fleets`: 409 unless the fleet is absent or settled `Down`.
    Create,
    /// `PUT /v1/fleets/{name}`: 404 when absent.
    Replace,
    /// A plugin's `PUT` (Spec L §3.1): create or replace; a settled `Down`
    /// resumes.
    Upsert,
}
```

Then, in `impl Daemon`, replace `apply` and `down` with the wrappers and the `_as` forms. `apply_as` is the body of today's `apply` with three edits, shown in full here so the implementer does not have to merge:

```rust
    /// Spec L §5: the CLI and a plugin never touch each other's fleets.
    /// `downing` is the one case an admin may override, with `force`.
    fn check_owner(
        name: &FleetName,
        owner: Option<&str>,
        caller: &Caller,
        downing: bool,
    ) -> Result<(), DaemonError> {
        match (caller, owner) {
            (Caller::Admin { force }, Some(p)) if !(downing && *force) => Err(
                DaemonError::Managed(format!("fleet {name} is managed by plugin {p}")),
            ),
            (Caller::Plugin(me), Some(p)) if p != me.as_str() => Err(DaemonError::Managed(
                format!("fleet {name} is managed by plugin {p}"),
            )),
            (Caller::Plugin(_), None) => Err(DaemonError::Managed(format!(
                "fleet {name} is not managed by a plugin"
            ))),
            _ => Ok(()),
        }
    }

    /// The admin API's apply: `POST` (`replace == false`) or `PUT`.
    pub async fn apply(
        &self,
        name: &FleetName,
        spec: FleetSpec,
        credentials: CredentialBundle,
        replace: bool,
    ) -> Result<FleetRecord, DaemonError> {
        let mode = if replace {
            ApplyMode::Replace
        } else {
            ApplyMode::Create
        };
        self.apply_as(name, spec, credentials, mode, &Caller::Admin { force: false })
            .await
    }

    /// `Create`: 409 unless the fleet is absent or settled `Down`.
    /// `Replace`: 404 when absent. `Upsert`: either. Every mode is
    /// answered before any plugin is called, so a refused apply leaves no
    /// plugin holding a config the fleet never took; and the owner rule
    /// (Spec L §5) is answered there too. A record a plugin creates
    /// carries that plugin as its owner.
    ///
    /// The activation diff's old side is the fleet's `Active` rows only
    /// (R24): a pair whose row is `Pending` or `Rejected` is offered to the
    /// plugin again by the next `apply`, even when its config is unchanged
    /// — a ready plugin gets an `activate` (and a rejection fails the
    /// apply like any other), a plugin that is not ready leaves it pending.
    pub async fn apply_as(
        &self,
        name: &FleetName,
        spec: FleetSpec,
        credentials: CredentialBundle,
        mode: ApplyMode,
        caller: &Caller,
    ) -> Result<FleetRecord, DaemonError> {
        Self::reject_reserved(name)?;
        if spec.name != name.as_str() {
            return Err(DaemonError::Invalid(format!(
                "spec.name {:?} does not match the fleet {name}",
                spec.name
            )));
        }
        Fleet::try_from(spec.clone()).map_err(|e| DaemonError::Invalid(e.to_string()))?;
        // Activation runs before the actor sees the spec (§16.2): a
        // rejection is a 400 and nothing lands. Held for the whole method
        // so two applies of the same fleet cannot interleave.
        let lock = self.fleet_lock(name);
        let _guard = lock.lock().await;
        // The fleet's rows *now*, not what its spec says: a `down` answers
        // before its teardown pass and has already dropped every row, so
        // the record's spec would name pairs that are gone.
        let rows_now = self.registry.rows_for_fleet(name);
        // Only the `Active` rows are the diff's old side (R24): a pending
        // or rejected pair is offered to the plugin again by the next
        // apply even when its config did not change.
        let old: Vec<Pair> = rows_now
            .iter()
            .filter(|(_, _, row)| row.activation.state == ActivationState::Active)
            .map(|(agent, plugin, row)| Pair {
                agent: agent.clone(),
                plugin: plugin.clone(),
                config: row.config.clone(),
            })
            .collect();
        let new =
            activation::pairs(name, &spec).map_err(|e| DaemonError::Invalid(e.to_string()))?;
        for p in &new {
            if !self.registry.is_installed(p.plugin.as_str()) {
                return Err(DaemonError::Invalid(format!(
                    "{}: no plugin {:?} is installed",
                    activation::config_path(&p.agent, &p.plugin),
                    p.plugin.as_str()
                )));
            }
        }
        // Whether this is a 409 (Create on a live fleet), a 404 (Replace
        // on an absent one) or the owner's 409 is decided *before* any
        // plugin is told anything: a rejected apply must not leave a
        // plugin holding a config the fleet never took. The per-fleet
        // lock is held, and it excludes the only other mutators of this
        // entry, so the write lock below sees the same answer.
        {
            let fleets = self.fleets.read().await;
            match fleets.get(name) {
                Some(h) => {
                    let current = h.status.borrow();
                    Self::check_owner(name, current.owner.as_deref(), caller, false)?;
                    if mode == ApplyMode::Create && !current.is_down() {
                        return Err(DaemonError::Conflict);
                    }
                }
                None if mode == ApplyMode::Replace => return Err(DaemonError::NotFound),
                None => {}
            }
        }
        let mut d = activation::diff(&old, &new);
        // A row the new spec no longer names goes, whatever its state; the
        // diff only saw the active ones.
        for (agent, plugin, _) in &rows_now {
            let named = new.iter().any(|p| &p.agent == agent && &p.plugin == plugin);
            let already = d.deactivate.iter().any(|(a, p)| a == agent && p == plugin);
            if !named && !already {
                d.deactivate.push((agent.clone(), plugin.clone()));
            }
        }
        d.deactivate.sort();
        // every new or changed pair on a ready plugin is offered its config
        // by `activate` alone: a changed pair is replaced in place, never
        // deactivated first (§16.2, §17.9), so a rejection leaves the
        // plugin's state for it untouched. The first rejection rolls back
        // the pairs already accepted — one that was active before gets its
        // old config re-activated, a new one is deactivated — and nothing
        // reaches the actor.
        let mut accepted: Vec<Pair> = Vec::new();
        let mut rows: Vec<(Pair, PluginActivation)> = Vec::new();
        for p in &d.activate {
            match self.registry.ready_addr(&p.plugin) {
                Some(addr) => {
                    let req = ActivateRequest {
                        agent: p.agent.to_string(),
                        config: p.config.clone(),
                    };
                    match self.client.activate(&addr, &req).await {
                        Ok(()) => {
                            accepted.push(p.clone());
                            rows.push((p.clone(), PluginActivation::active()));
                        }
                        Err(e) => {
                            let previous = |a: &Pair| {
                                old.iter()
                                    .find(|o| o.agent == a.agent && o.plugin == a.plugin)
                                    .cloned()
                            };
                            let mut restore = Vec::new();
                            for a in &accepted {
                                match previous(a) {
                                    Some(was) => restore.push(was),
                                    None => self.deactivate_pair(&a.agent, &a.plugin).await,
                                }
                            }
                            self.restore_pairs(&restore).await;
                            // The rollback may have written rows of its own
                            // and this apply returns before the tick at the
                            // end: no actor snapshot is behind it either.
                            self.bump();
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
        let handle = {
            let mut fleets = self.fleets.write().await;
            match fleets.get(name) {
                Some(h) => {
                    if mode == ApplyMode::Create && !h.status.borrow().is_down() {
                        return Err(DaemonError::Conflict);
                    }
                    h.clone()
                }
                None => {
                    if mode == ApplyMode::Replace {
                        return Err(DaemonError::NotFound);
                    }
                    let h = actor::spawn(
                        name.clone(),
                        FleetRecord::with_owner(spec.clone(), caller.owner()),
                        FleetSecrets::default(),
                        self.ports.clone(),
                        self.shared.clone(),
                        false,
                    );
                    tokio::spawn(Self::forward_changes(
                        h.status.clone(),
                        self.changes.clone(),
                    ));
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
        let record = rx
            .await
            .map_err(|_| DaemonError::Internal("fleet task dropped the request".into()))?;
        // only the pairs the new spec dropped: a changed pair kept its row
        // and was replaced in place above
        for (agent, plugin) in &d.deactivate {
            self.deactivate_pair(agent, plugin).await;
            self.registry.remove_row(agent, plugin);
        }
        for (p, activation) in rows {
            self.registry.set_row(
                &p.agent,
                &p.plugin,
                ActivationRow {
                    config: p.config,
                    activation,
                },
            );
        }
        self.bump();
        Ok(self.overlay(record))
    }

    /// The admin API's down, without `--force`.
    pub async fn down(
        &self,
        name: &FleetName,
        keep: Keep,
        purge: bool,
    ) -> Result<FleetRecord, DaemonError> {
        self.down_as(name, keep, purge, &Caller::Admin { force: false })
            .await
    }

    /// Owner rule (Spec L §5): a plugin downs only what it owns; the admin
    /// needs `force` for a managed fleet. The owner survives the down.
    pub async fn down_as(
        &self,
        name: &FleetName,
        keep: Keep,
        purge: bool,
        caller: &Caller,
    ) -> Result<FleetRecord, DaemonError> {
        Self::reject_reserved(name)?;
        let lock = self.fleet_lock(name);
        let _guard = lock.lock().await;
        let handle = self
            .fleets
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or(DaemonError::NotFound)?;
        Self::check_owner(name, handle.status.borrow().owner.as_deref(), caller, true)?;
        let (reply, rx) = oneshot::channel();
        handle
            .tx
            .send(Msg::Down { keep, purge, reply })
            .await
            .map_err(|_| DaemonError::Internal("fleet task is gone".into()))?;
        let record = rx
            .await
            .map_err(|_| DaemonError::Internal("fleet task dropped the request".into()))?;
        for (agent, plugin) in self.registry.remove_fleet(name) {
            self.deactivate_pair(&agent, &plugin).await;
        }
        self.bump();
        Ok(self.overlay(record))
    }

    /// `PUT /v1/plugin-host/fleets/{name}` (Spec L §3.1): a plugin's
    /// unresolved fleet file becomes a fleet the plugin owns. The name is
    /// checked first (reserved, and a `name` in the file must equal the
    /// path's), then the owner (cheaply, so a foreign fleet costs no
    /// resolve; `apply_as` checks again under the fleet's lock), then the
    /// file is resolved through the port (400 with the resolver's message,
    /// config path first), the operator's credentials are read now (500:
    /// an unreadable host home is the operator's problem, not the
    /// plugin's), and the spec is applied as an upsert.
    pub async fn manage_fleet(
        &self,
        plugin: &AgentName,
        name: &FleetName,
        file: serde_json::Value,
    ) -> Result<FleetRecord, DaemonError> {
        Self::reject_reserved(name)?;
        if let Some(n) = file.get("name").and_then(serde_json::Value::as_str)
            && n != name.as_str()
        {
            return Err(DaemonError::Invalid(format!(
                "name: {n:?} does not match the fleet {name}"
            )));
        }
        let caller = Caller::Plugin(plugin.clone());
        if let Some(current) = self.get(name).await {
            Self::check_owner(name, current.owner.as_deref(), &caller, false)?;
        }
        let resolver = self.ports.resolver.clone();
        let resolve_name = name.clone();
        let spec = tokio::task::spawn_blocking(move || resolver.resolve(&file, &resolve_name))
            .await
            .map_err(|e| DaemonError::Internal(e.to_string()))?
            .map_err(DaemonError::Invalid)?;
        let source = self.ports.credentials.clone();
        let credentials = tokio::task::spawn_blocking(move || source.load())
            .await
            .map_err(|e| DaemonError::Internal(e.to_string()))?
            .map_err(DaemonError::Internal)?;
        self.apply_as(name, spec, credentials, ApplyMode::Upsert, &caller)
            .await
    }
```

`sync_plugins` becomes:

```rust
    /// Reconciles the plugin set to `plugins.yaml`; `serve` calls it once
    /// at start and fails fast on an error, `plugin sync` on demand. A
    /// plugin the sync stopped cannot down the fleets it owns, and agents
    /// nobody can talk to are the wrong default (Spec L-6): every such
    /// fleet still up is downed here, every failure reported, and the
    /// sync goes on.
    pub async fn sync_plugins(&self) -> Result<SyncReport, PluginError> {
        let mut report = self.plugins.sync().await?;
        for plugin in &report.stopped {
            for record in self.plugin_fleets().await {
                if record.owner.as_deref() != Some(plugin.as_str())
                    || matches!(record.desired, Desired::Down { .. })
                {
                    continue;
                }
                let Ok(name) = FleetName::try_from(record.spec.name.clone()) else {
                    continue;
                };
                match self
                    .down_as(&name, Keep::default(), false, &Caller::Admin { force: true })
                    .await
                {
                    Ok(_) => report.downed.push(name.to_string()),
                    Err(e) => {
                        tracing::warn!(fleet = %name, plugin, "downing a removed plugin's fleet failed: {e}");
                        report.down_failed.push(format!("{name}: {e}"));
                    }
                }
            }
        }
        // `replace_plugins` drops the activation rows of every plugin the
        // sync removed, and the plugin fleet's own actor snapshot is not
        // forwarded to `changes` — so this registry write ticks like the
        // others, or `fleets/watch` keeps serving the removed rows. A
        // sync that fails resolving does so before it replaces anything;
        // the failures after `replace_plugins` are the actor being gone,
        // which is shutdown, when no watcher is left to tell.
        self.bump();
        Ok(report)
    }
```

`crates/balerix-server/src/api.rs`, `From<DaemonError>`: add `DaemonError::Managed(_) => StatusCode::CONFLICT,`.

`crates/balerix-server/src/lib.rs`: `pub use daemon::{ApplyMode, Caller, Daemon, DaemonError, DaemonHandler, HEALTH_INTERVAL, HelloObserver};`.

- [ ] **Step 5: Run the daemon tests, then the gate**

Run: `mise x -- cargo nextest run -p balerix-server daemon::` then `mise run check`
Expected: PASS. `check` also compiles `plugins_it.rs` and the binary, whose `Ports` literals Step 3 updated; `scripts/check-core-deps.sh` still sees seven members and a core `reqwest` with `json` alone (no dependency moved).

- [ ] **Step 6: Commit**

```bash
git add crates/balerix-server crates/balerix/src/wiring.rs crates/balerix/src/commands/serve.rs
git commit -m "feat(server): owned fleets, the caller rule and manage_fleet over the two new ports (Spec L §3.1, §4, §5)"
```

---

### Task 6: The two plugin-host routes and `force` on the admin down (`balerix-server`)

**Files:**
- Modify: `crates/balerix-server/src/plugin_api.rs` (router, two handlers), `crates/balerix-server/src/api.rs:321-339` (`delete_fleet`), `crates/balerix-server/src/store.rs` (one test), `crates/balerix-server/tests/support/mod.rs:157-208` (`world_with`)
- Create: `crates/balerix-server/tests/manage_it.rs`

**Interfaces:**
- Consumes: `Daemon::{manage_fleet, down_as}`, `Caller` (Task 5); `Capability::Manage`, `DownQuery.force` (Task 1).
- Produces: `PUT /v1/plugin-host/fleets/{name}` with body `{ "file": <object> }` → 200 `FleetRecord`; `DELETE /v1/plugin-host/fleets/{name}?keep_repos=&keep_sessions=&purge=&force=` → 200 `FleetRecord`; admin `DELETE /v1/fleets/{name}?…&force=true` on a managed fleet → 200; `support::world_with(extra: &[(&str, &str)]) -> World`; `api::down_flags` shared by both `DELETE` handlers.

- [ ] **Step 1: Write the failing integration test**

`crates/balerix-server/tests/support/mod.rs`: split `world()` into `world_with`:

```rust
/// A daemon with the chain handler, served on a port, with one package
/// `flow` declared (intercepts PreToolUse+Stop, observes Stop, needs
/// actions+kv) and one package `web` (observes SessionStart, needs
/// fleets+attach+workspace, serves routes).
pub async fn world() -> World {
    world_with(&[]).await
}

/// `world`, plus one package per `(name, manifest lines)` in `extra`,
/// declared after `flow` and `web`.
pub async fn world_with(extra: &[(&str, &str)]) -> World {
    let h = Harness::new(Duration::from_secs(3600));
    let dir = tempfile::tempdir().unwrap();
    write_plugin_package(
        &dir.path().join("flow-pkg"),
        "flow",
        "hooks: { intercept: [PreToolUse, Stop], observe: [Stop] }\nneeds: [actions, kv]\n",
    );
    write_plugin_package(
        &dir.path().join("web-pkg"),
        "web",
        "hooks: { observe: [SessionStart] }\nneeds: [fleets, attach, workspace]\nroutes: true\n",
    );
    let mut plugins_yaml = String::from(
        "plugins:\n  - name: flow\n    source: ./flow-pkg\n  - name: web\n    source: ./web-pkg\n",
    );
    for (name, manifest) in extra {
        write_plugin_package(&dir.path().join(format!("{name}-pkg")), name, manifest);
        plugins_yaml.push_str(&format!("  - name: {name}\n    source: ./{name}-pkg\n"));
    }
    std::fs::write(dir.path().join("plugins.yaml"), plugins_yaml).unwrap();
    // (the rest of today's `world` body, unchanged)
    …
}
```

Create `crates/balerix-server/tests/manage_it.rs`:

```rust
//! Spec L §3: `PUT`/`DELETE /v1/plugin-host/fleets/{name}` over HTTP —
//! the capability gate, the owner rule from both sides, the admin's
//! `force`, and the body shapes the route refuses.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use balerix_api::{
    AgentSettings, CrewSpec, FleetPhase, FleetRecord, FleetRequest, FleetSpec, FleetSummary,
    GitSettings,
};
use balerix_core::plugin_id;
use balerix_plugin_sdk::{Env, Host, Plugin, bind, run};
use serde_json::{Value, json};
use support::{World, world_with};

struct Silent;
impl Plugin for Silent {}

async fn token(w: &World, plugin: &str) -> String {
    w.daemon
        .hook_secret(&plugin_id(&plugin.parse().unwrap()))
        .await
        .unwrap()
}

/// Starts a silent SDK plugin under `name` and says hello with its token.
async fn start_silent(w: &World, name: &str) -> Host {
    let env = Env {
        api_url: w.api.base.clone(),
        name: name.into(),
        token: token(w, name).await,
        scratch: w.dir.path().join("s"),
    };
    let (listener, listen) = bind().await.unwrap();
    let tok = env.token.clone();
    tokio::spawn(async move { run(listener, Arc::new(Silent), &tok).await });
    let host = Host::new(env).unwrap();
    host.hello("0.1.0", &listen).await.unwrap();
    host
}

fn spec(name: &str) -> FleetSpec {
    FleetSpec {
        name: name.into(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/x".into(),
                git_ref: "main".into(),
                git: GitSettings::default(),
                agents: BTreeMap::from([("a".to_string(), AgentSettings::default())]),
                ..Default::default()
            },
        )]),
        ..Default::default()
    }
}

fn file(name: &str) -> Value {
    json!({
        "apiVersion": "balerix/v1", "kind": "Fleet", "name": name,
        "crews": { "c": { "repo": "acme/x", "agents": { "a": {} } } }
    })
}

async fn wait_for(w: &World, name: &str, pred: impl Fn(&FleetRecord) -> bool) -> FleetRecord {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(r) = w.daemon.get(&name.parse().unwrap()).await
                && pred(&r)
            {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition not reached")
}

const DOWN: &str = "keep_repos=false&keep_sessions=false&purge=false&force=false";
const FORCED: &str = "keep_repos=false&keep_sessions=false&purge=false&force=true";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plugin_with_manage_applies_and_downs_a_fleet_it_owns() {
    let w = world_with(&[("gh", "needs: [fleets, manage]\n")]).await;
    let _gh = start_silent(&w, "gh").await;
    let _web = start_silent(&w, "web").await;
    let gh_tok = token(&w, "gh").await;
    let web_tok = token(&w, "web").await;
    w.h.resolver.set(Ok(spec("f")));

    // apply: owned, resolved through the port, running
    let (s, v) = w.api.plugin(
        &gh_tok,
        "PUT",
        "/v1/plugin-host/fleets/f",
        Some(&json!({ "file": file("f") })),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!((v["owner"].as_str(), v["generation"].as_u64()), (Some("gh"), Some(1)));
    assert_eq!(w.h.resolver.calls(), vec![("f".to_string(), file("f"))]);
    wait_for(&w, "f", |r| r.status.observed_generation == 1).await;
    let (s, v) = w.api.plugin(&gh_tok, "GET", "/v1/plugin-host/fleets/f", None);
    assert_eq!((s, v["owner"].as_str()), (200, Some("gh")));
    let (_, v) = w.api.admin("GET", "/v1/fleets", None);
    let rows: Vec<FleetSummary> = serde_json::from_value(v).unwrap();
    assert_eq!(rows[0].managed_by.as_deref(), Some("gh"));

    // the capability gate
    let (s, v) = w.api.plugin(
        &web_tok,
        "PUT",
        "/v1/plugin-host/fleets/f",
        Some(&json!({ "file": file("f") })),
    );
    assert_eq!(s, 403, "{v}");
    assert_eq!(
        v["error"],
        "capability \"manage\" not declared in balerix-plugin.yaml"
    );
    let (s, _) = w
        .api
        .plugin(&web_tok, "DELETE", &format!("/v1/plugin-host/fleets/f?{DOWN}"), None);
    assert_eq!(s, 403);
    let (s, _) = w.api.plugin(
        "nope",
        "PUT",
        "/v1/plugin-host/fleets/f",
        Some(&json!({ "file": file("f") })),
    );
    assert_eq!(s, 401);

    // a second apply replaces in place
    let (s, v) = w.api.plugin(
        &gh_tok,
        "PUT",
        "/v1/plugin-host/fleets/f",
        Some(&json!({ "file": file("f") })),
    );
    assert_eq!((s, v["generation"].as_u64()), (200, Some(2)));

    // the admin routes refuse a managed fleet; `force` takes it down
    let req = json!(FleetRequest {
        spec: spec("f"),
        credentials: Default::default()
    });
    let (s, v) = w.api.admin("POST", "/v1/fleets", Some(&req));
    assert_eq!((s, v["error"].as_str()), (409, Some("fleet f is managed by plugin gh")));
    let (s, v) = w.api.admin("PUT", "/v1/fleets/f", Some(&req));
    assert_eq!((s, v["error"].as_str()), (409, Some("fleet f is managed by plugin gh")));
    let (s, v) = w.api.admin("DELETE", &format!("/v1/fleets/f?{DOWN}"), None);
    assert_eq!((s, v["error"].as_str()), (409, Some("fleet f is managed by plugin gh")));
    let (s, v) = w.api.admin("DELETE", &format!("/v1/fleets/f?{FORCED}"), None);
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["owner"], "gh", "a forced down keeps the owner");
    wait_for(&w, "f", |r| r.status.phase == FleetPhase::Down).await;

    // the owner resumes it and downs it itself
    let (s, v) = w.api.plugin(
        &gh_tok,
        "PUT",
        "/v1/plugin-host/fleets/f",
        Some(&json!({ "file": file("f") })),
    );
    assert_eq!((s, v["generation"].as_u64()), (200, Some(3)), "{v}");
    let (s, v) = w
        .api
        .plugin(&gh_tok, "DELETE", &format!("/v1/plugin-host/fleets/f?{DOWN}"), None);
    assert_eq!(s, 200, "{v}");
    assert_eq!((v["desired"]["state"].as_str(), v["owner"].as_str()), (Some("down"), Some("gh")));

    // a fleet the CLI created is not the plugin's to apply or down
    let g = json!(FleetRequest {
        spec: spec("g"),
        credentials: Default::default()
    });
    let (s, _) = w.api.admin("POST", "/v1/fleets", Some(&g));
    assert_eq!(s, 200);
    let (s, v) = w.api.plugin(
        &gh_tok,
        "PUT",
        "/v1/plugin-host/fleets/g",
        Some(&json!({ "file": file("g") })),
    );
    assert_eq!((s, v["error"].as_str()), (409, Some("fleet g is not managed by a plugin")));
    let (s, v) = w
        .api
        .plugin(&gh_tok, "DELETE", &format!("/v1/plugin-host/fleets/g?{DOWN}"), None);
    assert_eq!((s, v["error"].as_str()), (409, Some("fleet g is not managed by a plugin")));
    assert_eq!(w.h.resolver.calls().len(), 3, "a foreign fleet was never resolved");
}

/// Review focus 3 and 5: what the route refuses before the daemon sees a file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_manage_routes_refuse_bad_names_bodies_and_flags() {
    let w = world_with(&[("gh", "needs: [manage]\n")]).await;
    let _gh = start_silent(&w, "gh").await;
    let gh_tok = token(&w, "gh").await;
    let put = |path: &str, body: Value| w.api.plugin(&gh_tok, "PUT", path, Some(&body));

    // `watch` is a route, never a fleet: axum answers the static route's
    // method set, and the daemon's reserved-name rule stands behind it
    let (s, _) = put("/v1/plugin-host/fleets/watch", json!({ "file": file("watch") }));
    assert_eq!(s, 405);
    let (s, v) = put("/v1/plugin-host/fleets/balerix", json!({ "file": file("balerix") }));
    assert_eq!(s, 400, "{v}");
    assert!(v["error"].as_str().unwrap().starts_with("name:"), "{v}");
    let (s, v) = put("/v1/plugin-host/fleets/Not-Valid", json!({ "file": file("f") }));
    assert_eq!(s, 400, "{v}");
    assert!(v["error"].as_str().unwrap().starts_with("name:"), "{v}");
    let (s, v) = put("/v1/plugin-host/fleets/f", json!({}));
    assert_eq!(s, 400, "{v}");
    assert!(v["error"].as_str().unwrap().contains("file"), "{v}");
    let (s, v) = put("/v1/plugin-host/fleets/f", json!({ "file": 3 }));
    assert_eq!((s, v["error"].as_str()), (400, Some("file: expected a mapping")));
    let (s, v) = put("/v1/plugin-host/fleets/f", json!({ "file": file("f"), "x": 1 }));
    assert_eq!(s, 400, "{v}");
    let (s, v) = put("/v1/plugin-host/fleets/f", json!({ "file": file("g") }));
    assert_eq!(
        (s, v["error"].as_str()),
        (400, Some("name: \"g\" does not match the fleet f"))
    );
    assert!(w.h.resolver.calls().is_empty(), "nothing above reached the resolver");

    // the resolver's refusal, verbatim, config path first
    let bad = "crews.c.agents.a.tools.node: expected an exact version, got \"22\" (try: mise latest node@22)";
    w.h.resolver.set(Err(bad.into()));
    let (s, v) = put("/v1/plugin-host/fleets/f", json!({ "file": file("f") }));
    assert_eq!((s, v["error"].as_str()), (400, Some(bad)));
    assert!(w.daemon.get(&"f".parse().unwrap()).await.is_none());

    // the down flags' rule, and a fleet that does not exist
    let (s, v) = w.api.plugin(
        &gh_tok,
        "DELETE",
        "/v1/plugin-host/fleets/f?keep_repos=true&keep_sessions=false&purge=true&force=false",
        None,
    );
    assert_eq!(
        (s, v["error"].as_str()),
        (400, Some("purge cannot be combined with keep flags"))
    );
    let (s, v) = w
        .api
        .plugin(&gh_tok, "DELETE", &format!("/v1/plugin-host/fleets/f?{DOWN}"), None);
    assert_eq!((s, v["error"].as_str()), (404, Some("fleet not found")));
}
```

And the store test, in `crates/balerix-server/src/store.rs` `mod tests`:

```rust
    /// Spec L §5: the owner is in `fleet.json`, and only when there is one.
    #[test]
    fn the_owner_survives_the_file_store() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let mut owned = record("a");
        owned.owner = Some("github".into());
        s.put(&owned, &FleetSecrets::default()).unwrap();
        let text = std::fs::read_to_string(s.fleet_dir(&name("a")).join("fleet.json")).unwrap();
        assert!(text.contains("\"owner\": \"github\""), "{text}");
        assert_eq!(s.load_all().unwrap()[0].0.owner.as_deref(), Some("github"));
        s.put(&record("b"), &FleetSecrets::default()).unwrap();
        let text = std::fs::read_to_string(s.fleet_dir(&name("b")).join("fleet.json")).unwrap();
        assert!(!text.contains("owner"), "an unowned record has no key: {text}");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-server --test manage_it` and `mise x -- cargo nextest run -p balerix-server store::`
Expected: `manage_it` fails at the first `PUT` with 405 (no `put` on the route); the store test passes already (the record type carries the owner since Task 1) — keep it, it pins the on-disk shape.

- [ ] **Step 3: Implement the routes**

`crates/balerix-server/src/api.rs`: factor the flag rule out of `delete_fleet` so both `DELETE`s share it, and pass the caller:

```rust
/// The `DownQuery` of a `DELETE`, with the one rule both routes apply:
/// purge excludes keep.
pub(crate) fn down_flags(q: Result<Query<DownQuery>, QueryRejection>) -> Result<DownQuery, ApiError> {
    let Query(q) = q.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    if q.purge && (q.keep_repos || q.keep_sessions) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "purge cannot be combined with keep flags",
        ));
    }
    Ok(q)
}

async fn delete_fleet(
    State(state): State<AppState>,
    name: Result<Path<String>, PathRejection>,
    q: Result<Query<DownQuery>, QueryRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    let q = down_flags(q)?;
    let name = fleet_name(&path_name(name)?)?;
    let keep = Keep {
        repos: q.keep_repos,
        sessions: q.keep_sessions,
    };
    // Spec L §5: a managed fleet needs `--force` from the admin side.
    Ok(Json(
        state
            .daemon
            .down_as(&name, keep, q.purge, &Caller::Admin { force: q.force })
            .await?,
    ))
}
```

(import `Caller` from `crate::daemon`.)

`crates/balerix-server/src/plugin_api.rs`: imports gain `DownQuery` (from `balerix_api`), `Keep` (from `balerix_core`), `crate::api::down_flags`, `crate::daemon::Caller`; the router line becomes

```rust
        .route(
            "/v1/plugin-host/fleets/{name}",
            get(get_fleet).put(put_fleet).delete(delete_fleet),
        )
```

and, after `get_fleet`:

```rust
/// Body of `PUT fleets/{name}` (Spec L §3.1): the YAML fleet file's
/// structure as JSON. Nothing else: the daemon resolves and reads the
/// credentials itself.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FleetFileBody {
    file: Value,
}

/// The fleet of a manage route. A bad name is a 400 that names it: the
/// daemon has nothing to be "not found" for a name it never accepts, and
/// the plugin built it.
fn manage_name(name: Result<Path<String>, PathRejection>) -> Result<FleetName, ApiError> {
    let Path(name) = name.map_err(|e| ApiError::new(e.status(), e.body_text()))?;
    name.parse().map_err(|e: balerix_core::NameError| {
        ApiError::new(StatusCode::BAD_REQUEST, format!("name: {e}"))
    })
}

/// `PUT fleets/{name}` (Spec L §3.1): an unresolved fleet file becomes a
/// fleet this plugin owns. `manage` gates it; the name, the owner and
/// the file's resolution are `Daemon::manage_fleet`'s.
async fn put_fleet(
    State(state): State<AppState>,
    headers: HeaderMap,
    name: Result<Path<String>, PathRejection>,
    body: Result<Json<FleetFileBody>, JsonRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    let plugin = caller(&state, &headers, Capability::Manage).await?;
    let name = manage_name(name)?;
    let Json(body) = body.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    if !body.file.is_object() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "file: expected a mapping",
        ));
    }
    Ok(Json(
        state.daemon.manage_fleet(&plugin, &name, body.file).await?,
    ))
}

/// `DELETE fleets/{name}?…` (Spec L §3.2): the admin `DELETE`'s flags,
/// for a fleet this plugin owns. `force` is read and ignored: ownership
/// is the rule here.
async fn delete_fleet(
    State(state): State<AppState>,
    headers: HeaderMap,
    name: Result<Path<String>, PathRejection>,
    q: Result<Query<DownQuery>, QueryRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    let plugin = caller(&state, &headers, Capability::Manage).await?;
    let name = manage_name(name)?;
    let q = down_flags(q)?;
    let keep = Keep {
        repos: q.keep_repos,
        sessions: q.keep_sessions,
    };
    Ok(Json(
        state
            .daemon
            .down_as(&name, keep, q.purge, &Caller::Plugin(plugin))
            .await?,
    ))
}
```

- [ ] **Step 4: Run the tests, then the gate**

Run: `mise x -- cargo nextest run -p balerix-server` then `mise run check`
Expected: PASS. If the `watch` case answers 400 instead of 405 (axum matched the parameter route), keep the daemon's answer and change the assertion to `assert_eq!(s, 400)` with the daemon's `name:` message — either way no fleet named `watch` exists afterwards; add `assert!(w.daemon.get(&"watch".parse().unwrap()).await.is_none())` in both cases.

- [ ] **Step 5: Commit**

```bash
git add crates/balerix-server
git commit -m "feat(server): PUT and DELETE /v1/plugin-host/fleets/{name} behind manage; down --force on the admin route (Spec L §3)"
```

---

### Task 7: The SDK, its fake and the protocol fixtures (`balerix-plugin-sdk`, `docs/plugin-protocol*`)

**Files:**
- Modify: `crates/balerix-plugin-sdk/src/host.rs`, `crates/balerix-plugin-sdk/src/testing.rs`, `crates/balerix-plugin-sdk/tests/conformance.rs:25-30,148-314`, `docs/plugin-protocol.md` §1, §3, §6
- Create: `docs/plugin-protocol/fleet-put.json`, `docs/plugin-protocol/fleet-put-rejected.json`, `docs/plugin-protocol/fleet-delete.json`

**Interfaces:**
- Produces:
  ```rust
  impl Host {
      pub async fn apply_fleet(&self, name: &str, file: &Value) -> Result<FleetRecord, SdkError>;
      pub async fn down_fleet(&self, name: &str, query: &DownQuery) -> Result<FleetRecord, SdkError>;
  }
  impl FakeHost {
      pub fn applied_fleets(&self) -> Vec<(String, Value)>;      // (name, file) per PUT, refused ones included
      pub fn downed_fleets(&self) -> Vec<(String, DownQuery)>;   // per DELETE
      pub fn fail_manage(&self, answer: Option<(u16, &str)>);     // while Some, both routes answer that status and message
  }
  ```
  `FakeHost`'s `PUT` answers the record it already holds under that name (so `set_fleets` shapes the answer), else a fresh record owned by `"plugin"`, and pushes the list to every watch; its `DELETE` marks the record `Down` (desired and phase) or answers 404 `fleet not found`.

- [ ] **Step 1: Write the fixtures**

`docs/plugin-protocol/fleet-put.json`:

```json
{
  "route": "PUT /v1/plugin-host/fleets/gh-acme-api",
  "direction": "plugin-to-daemon",
  "request": {
    "file": {
      "apiVersion": "balerix/v1",
      "kind": "Fleet",
      "name": "gh-acme-api",
      "crews": {
        "repo": {
          "repo": "acme/api",
          "agents": { "issue-12": { "branch": "feature/issue-12" } }
        }
      }
    }
  },
  "status": 200,
  "response": {
    "spec": {
      "name": "gh-acme-api",
      "crews": {
        "repo": {
          "repo": "acme/api",
          "ref": "main",
          "git": { "push": true, "auth": "gh" },
          "agents": {
            "issue-12": {
              "claude": { "settings": {}, "args": [], "resume": false, "binary": "claude" },
              "sandbox": {},
              "tools": {},
              "env": {},
              "runner": { "type": "tmux" },
              "plugins": {},
              "branch": "feature/issue-12"
            }
          }
        }
      }
    },
    "owner": "github",
    "generation": 1,
    "desired": { "state": "up" },
    "stopped": [],
    "status": { "generation": 1, "observed_generation": 0, "phase": "pending", "agents": {} }
  }
}
```

`docs/plugin-protocol/fleet-put-rejected.json`:

```json
{
  "route": "PUT /v1/plugin-host/fleets/gh-acme-api",
  "direction": "plugin-to-daemon",
  "request": {
    "file": {
      "apiVersion": "balerix/v1",
      "kind": "Fleet",
      "name": "gh-acme-api",
      "crews": {
        "repo": {
          "repo": "acme/api",
          "agents": { "issue-12": { "tools": { "node": "22" } } }
        }
      }
    }
  },
  "status": 400,
  "response": {
    "error": "crews.repo.agents.issue-12.tools.node: expected an exact version, got \"22\" (try: mise latest node@22)"
  }
}
```

`docs/plugin-protocol/fleet-delete.json`: the `fleet-put.json` response with `desired` and `status.phase` changed:

```json
{
  "route": "DELETE /v1/plugin-host/fleets/gh-acme-api?keep_repos=false&keep_sessions=false&purge=false&force=false",
  "direction": "plugin-to-daemon",
  "request": null,
  "status": 200,
  "response": {
    "spec": { …exactly the `spec` of fleet-put.json… },
    "owner": "github",
    "generation": 1,
    "desired": { "state": "down", "keep": { "repos": false, "sessions": false }, "purge": false },
    "stopped": [],
    "status": { "generation": 1, "observed_generation": 0, "phase": "down", "agents": {} }
  }
}
```

(copy the `spec` object verbatim from `fleet-put.json`; the two files must deserialize to `FleetRecord`s that differ only in `desired` and `status.phase`.)

- [ ] **Step 2: Write the failing conformance and fake-host tests**

`crates/balerix-plugin-sdk/tests/conformance.rs`: the count in `fixtures()` becomes `26`. Add `DownQuery` to the `balerix_api` import. At the end of `the_host_sends_every_plugin_to_daemon_fixture_and_reads_the_answer`, after the workspace block:

```rust
    // Spec L: the manage routes. The fake answers the record it holds
    // under the name, so `set_fleets` shapes the answer to the fixture.
    let put_record: FleetRecord =
        serde_json::from_value(fx["fleet-put"]["response"].clone()).unwrap();
    fake.set_fleets(vec![put_record]);
    let file = fx["fleet-put"]["request"]["file"].clone();
    assert_eq!(
        serde_json::to_value(host.apply_fleet("gh-acme-api", &file).await.unwrap()).unwrap(),
        fx["fleet-put"]["response"]
    );
    assert_eq!(
        fake.applied_fleets(),
        vec![("gh-acme-api".to_string(), file.clone())]
    );
    let rejected = fx["fleet-put-rejected"]["response"]["error"]
        .as_str()
        .unwrap();
    fake.fail_manage(Some((400, rejected)));
    let e = host
        .apply_fleet("gh-acme-api", &fx["fleet-put-rejected"]["request"]["file"])
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), format!("daemon: HTTP 400: {rejected}"));
    let resp = c
        .put(format!("{}/v1/plugin-host/fleets/gh-acme-api", fake.url))
        .bearer_auth("tok")
        .json(&fx["fleet-put-rejected"]["request"])
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        fx["fleet-put-rejected"]["status"].as_u64().unwrap() as u16
    );
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        fx["fleet-put-rejected"]["response"]
    );
    fake.fail_manage(None);
    let q = DownQuery::default();
    assert_eq!(
        fx["fleet-delete"]["route"],
        format!("DELETE /v1/plugin-host/fleets/gh-acme-api?{}", q.to_query_string()),
        "the fixture's route carries every flag, as the SDK sends them"
    );
    assert_eq!(
        serde_json::to_value(host.down_fleet("gh-acme-api", &q).await.unwrap()).unwrap(),
        fx["fleet-delete"]["response"]
    );
    assert_eq!(fake.downed_fleets(), vec![("gh-acme-api".to_string(), q)]);
    let e = host.down_fleet("nope", &q).await.unwrap_err();
    assert_eq!(e.to_string(), "daemon: HTTP 404: fleet not found");
```

In `crates/balerix-plugin-sdk/src/host.rs` `mod tests`, extend `every_route_round_trips_through_the_fake_host` at its end:

```rust
        // Spec L: a fleet the plugin applies is watched like any other
        let file = json!({ "apiVersion": "balerix/v1", "kind": "Fleet", "crews": {} });
        let rec = host.apply_fleet("billing", &file).await.unwrap();
        assert_eq!((rec.name(), rec.owner.as_deref()), ("billing", Some("plugin")));
        assert_eq!(host.fleets().await.unwrap().len(), 2, "the fake holds it now");
        assert_eq!(fake.applied_fleets(), vec![("billing".to_string(), file)]);
        let rec = host
            .down_fleet("billing", &balerix_api::DownQuery::default())
            .await
            .unwrap();
        assert!(rec.is_down());
        assert_eq!(fake.downed_fleets().len(), 1);
        fake.fail_manage(Some((409, "fleet billing is managed by plugin other")));
        let e = host.apply_fleet("billing", &json!({})).await.unwrap_err();
        assert_eq!(
            e.to_string(),
            "daemon: HTTP 409: fleet billing is managed by plugin other"
        );
        assert_eq!(fake.applied_fleets().len(), 2, "a refused apply is still recorded");
```

- [ ] **Step 3: Run them to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-plugin-sdk`
Expected: compile errors — no `apply_fleet`, `down_fleet`, `applied_fleets`, `downed_fleets`, `fail_manage`.

- [ ] **Step 4: Implement the `Host` methods**

`crates/balerix-plugin-sdk/src/host.rs`: import `DownQuery` from `balerix_api` and `serde_json::{Value, json}`; add

```rust
/// `apply_fleet` alone: the daemon resolves the file and then queues the
/// apply behind the fleet actor's current pass, which may be cloning a
/// repository or installing tools.
const APPLY_TIMEOUT: Duration = Duration::from_secs(120);
```

and, after `fleet`:

```rust
    /// `PUT fleets/{name}` (Spec L §3.1): applies an unresolved fleet
    /// file — the YAML fleet file's structure as JSON — as a fleet this
    /// plugin owns, and answers the record. Needs `manage`. 400 with the
    /// resolver's message (config path first) when the file does not
    /// resolve; 409 `fleet <name> is managed by plugin <p>` or `… is not
    /// managed by a plugin` when the name belongs to someone else. The
    /// call returns before the fleet is ready: watch `fleets/watch` or
    /// poll `fleet` for that.
    pub async fn apply_fleet(&self, name: &str, file: &Value) -> Result<FleetRecord, SdkError> {
        self.json(
            self.http
                .put(self.url(&format!("fleets/{name}")))
                .timeout(APPLY_TIMEOUT)
                .json(&json!({ "file": file })),
        )
        .await
    }

    /// `DELETE fleets/{name}?…` (Spec L §3.2): downs a fleet this plugin
    /// applied, with the admin `DELETE`'s flags (`force` is ignored
    /// there). 409 for a fleet it does not own, 404 for none.
    pub async fn down_fleet(
        &self,
        name: &str,
        query: &DownQuery,
    ) -> Result<FleetRecord, SdkError> {
        self.json(
            self.http
                .delete(self.url(&format!("fleets/{name}?{}", query.to_query_string()))),
        )
        .await
    }
```

- [ ] **Step 5: Implement the fake**

`crates/balerix-plugin-sdk/src/testing.rs`: imports gain `Desired, DownQuery, FleetPhase, FleetSpec, Keep` from `balerix_api` and `axum::extract::rejection::JsonRejection`. `Inner` gains

```rust
    /// `PUT fleets/{name}`: `(name, file)`, refused calls included.
    applied: Mutex<Vec<(String, Value)>>,
    /// `DELETE fleets/{name}`: `(name, flags)`.
    downed: Mutex<Vec<(String, DownQuery)>>,
    /// `fail_manage`: while set, both manage routes answer this.
    manage_failure: Mutex<Option<(u16, String)>>,
```

initialised in `start` as `Mutex::new(Vec::new())`, `Mutex::new(Vec::new())`, `Mutex::new(None)`. Accessors, beside `fail_actions`:

```rust
    pub fn applied_fleets(&self) -> Vec<(String, Value)> {
        self.inner
            .applied
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn downed_fleets(&self) -> Vec<(String, DownQuery)> {
        self.inner
            .downed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// While `Some((status, message))`, `PUT` and `DELETE fleets/{name}`
    /// answer that — the daemon refusing a file (400) or a name that is
    /// someone else's (409). The call is still recorded.
    pub fn fail_manage(&self, answer: Option<(u16, &str)>) {
        *self
            .inner
            .manage_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = answer.map(|(s, m)| (s, m.to_string()));
    }
```

The router line for `fleets/{name}` becomes `get(fleet).put(put_fleet).delete(delete_fleet)`. Handlers, after `fleet`:

```rust
#[derive(Deserialize)]
struct FleetFileBody {
    file: Value,
}

/// `Some(response)` while `fail_manage` is set.
fn manage_failure(inner: &Inner) -> Option<Response> {
    let (status, message) = inner
        .manage_failure
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()?;
    Some(error(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        message,
    ))
}

/// Spec L §3.1 as a fake: the record already held under `name` (what
/// `set_fleets` put there) is the answer, else a fresh one owned by
/// `plugin`; either way the list goes out to every watch.
async fn put_fleet(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
    body: Result<Json<FleetFileBody>, JsonRejection>,
) -> Response {
    if let Some(resp) = unauthorized(&inner, &headers) {
        return resp;
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    inner
        .applied
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((name.clone(), body.file));
    if let Some(resp) = manage_failure(&inner) {
        return resp;
    }
    let record = {
        let mut fleets = inner.fleets.lock().unwrap_or_else(|e| e.into_inner());
        match fleets.iter().find(|f| f.name() == name) {
            Some(f) => f.clone(),
            None => {
                let r = FleetRecord::with_owner(
                    FleetSpec {
                        name: name.clone(),
                        ..Default::default()
                    },
                    Some("plugin".into()),
                );
                fleets.push(r.clone());
                r
            }
        }
    };
    inner.fleets_changed.send_modify(|n| *n += 1);
    Json(record).into_response()
}

/// Spec L §3.2 as a fake: the record goes `Down` (desired and phase at
/// once) and is answered; a name the fake does not hold is 404.
async fn delete_fleet(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
    Query(q): Query<DownQuery>,
) -> Response {
    if let Some(resp) = unauthorized(&inner, &headers) {
        return resp;
    }
    inner
        .downed
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((name.clone(), q));
    if let Some(resp) = manage_failure(&inner) {
        return resp;
    }
    let record = {
        let mut fleets = inner.fleets.lock().unwrap_or_else(|e| e.into_inner());
        match fleets.iter_mut().find(|f| f.name() == name) {
            Some(f) => {
                f.desired = Desired::Down {
                    keep: Keep {
                        repos: q.keep_repos,
                        sessions: q.keep_sessions,
                    },
                    purge: q.purge,
                };
                f.status.phase = FleetPhase::Down;
                f.clone()
            }
            None => return error(StatusCode::NOT_FOUND, "fleet not found"),
        }
    };
    inner.fleets_changed.send_modify(|n| *n += 1);
    Json(record).into_response()
}
```

- [ ] **Step 6: Document the routes**

`docs/plugin-protocol.md`:

- §1 line 17-18: add `fleet-put-rejected.json` to the list of error fixtures.
- §3 intro: the capability list becomes `(fleets, actions, attach, kv, workspace, manage)`.
- §3 table, after the `GET fleets/watch` row:

```
| `PUT fleets/{name}` | `manage` | `{ file }` — the fleet file's structure as JSON (`apiVersion`, `kind`, `name`?, `defaults`, `crews`) | `FleetRecord`, `owner` set to this plugin | 200 | `fleet-put.json` |
| `PUT fleets/{name}`, file does not resolve | `manage` | same | `{ error }`, config path first | 400 | `fleet-put-rejected.json` |
| `PUT`/`DELETE fleets/{name}`, owned by another plugin or by the CLI | `manage` | — | `{ "error": "fleet <name> is managed by plugin <p>" }` or `{ "error": "fleet <name> is not managed by a plugin" }` | 409 | (asserted by `crates/balerix-server/tests/manage_it.rs`, §6) |
| `DELETE fleets/{name}?keep_repos=&keep_sessions=&purge=&force=` | `manage` | — | `FleetRecord` | 200 | `fleet-delete.json` |
```

- The `FleetRecord` paragraph after the table: `A FleetRecord is { spec: { name, crews }, owner?, generation, … }`; add one sentence: `owner` is present when a plugin manages the fleet (Spec L §5) and is the plugin's name.
- A new paragraph after **Workspace**:

```
**Managed fleets** (Spec L). `PUT fleets/{name}` takes the unresolved
fleet file — exactly what `balerix up -f` reads, as JSON — and does what
`up` does on the daemon: folds the host's `claude.settings` in, resolves
every agent, reads the operator's Claude credentials and gh token from
the daemon's host home, and applies. The plugin never sees credentials.
A `name` in the file must equal the path's (400 otherwise); `balerix`
and `watch` are refused. The first `PUT` of a name creates the record
with this plugin as its `owner`; every later `PUT` is a full replace and
must come from the same plugin; the CLI's `up`/`update`/`down` refuse an
owned fleet (409; `balerix down --force` is the operator's override).
The call returns once the spec is applied, not when the fleet is ready:
watch `fleets/watch`. `DELETE` takes the admin `DELETE`'s flags; `force`
is ignored here. `plugin remove` downs every fleet the plugin owned. An
agent's `branch` setting names an existing remote branch to work on
(`crews.<c>.agents.<a>.branch`, validated as `git check-ref-format
--branch` would); the diff base stays the crew's `ref`.
```

- §6: "twenty-three" becomes "twenty-six"; the last paragraph names `crates/balerix-server/tests/manage_it.rs` for the manage routes' 403 and 409s.

- [ ] **Step 7: Run the SDK tests and the gate**

Run: `mise x -- cargo nextest run -p balerix-plugin-sdk` then `mise run check`
Expected: PASS. If `fleet-put.json`'s response does not round-trip (the assertion prints both values), fix the fixture's `spec` to what `serde_json::to_value(FleetRecord)` prints — the record type is the source of truth, the fixture documents it.

- [ ] **Step 8: Commit**

```bash
git add crates/balerix-plugin-sdk docs/plugin-protocol docs/plugin-protocol.md
git commit -m "feat(sdk): Host::apply_fleet and down_fleet, the FakeHost routes and three conformance fixtures (Spec L §3.3)"
```

---

### Task 8: The CLI: the owner in `list`/`status`, `down --force`, `plugin remove` (`balerix`)

**Files:**
- Modify: `crates/balerix/src/cli.rs:78-95` (`DownArgs`), `crates/balerix/src/commands/fleet.rs:56-102,238-267` (renderers, `down_command`), `crates/balerix/src/commands/plugin.rs:54-69,219-246` (`render_sync`, `remove_command`), `crates/balerix/tests/cli_fleet.rs`

**Interfaces:**
- Consumes: `FleetRecord.owner`, `FleetSummary.managed_by`, `DownQuery.force`, `SyncReport.{downed,down_failed}` (Task 1); `Daemon::apply_as`, `ApplyMode`, `Caller` (Task 5, for the test's seed).
- Produces: `balerix down --force`; `balerix list` with a `MANAGED BY` column (`-` when none); `balerix status` whose first line ends in `  managed by <plugin>` for an owned fleet; `balerix plugin remove <p>` printing `fleet <f>: down` per fleet the daemon downed (`fleet <f>: purged` with `--purge`), and `fleet <f>: <error> (down failed)` per failure.

- [ ] **Step 1: Write the failing unit tests**

`crates/balerix/src/commands/fleet.rs`, in `status_and_list_render_aligned_tables`, the two expected strings become

```rust
        assert_eq!(
            render_status(&r),
            "payments  degraded  generation 3 (observed 3)\n\
             AGENT                   PHASE     RESTARTS  PLUGINS                    MESSAGE\n\
             payments/backend/alice  ready     0         flow=active\n\
             payments/backend/bob    starting  1         flow=rejected,web=pending  exited with status 1\n"
        );
        let rows = vec![r.summary()];
        assert_eq!(
            render_list(&rows),
            "NAME      PHASE     GEN  OBSERVED  AGENTS  MANAGED BY\n\
             payments  degraded  3    3         2       -\n"
        );
        assert_eq!(render_list(&[]), "no fleets\n");
        // Spec L §5: an owned fleet says so on its first line and in its row
        r.owner = Some("github".into());
        assert!(
            render_status(&r).starts_with(
                "payments  degraded  generation 3 (observed 3)  managed by github\n"
            ),
            "{}",
            render_status(&r)
        );
        assert_eq!(
            render_list(&[r.summary()]),
            "NAME      PHASE     GEN  OBSERVED  AGENTS  MANAGED BY\n\
             payments  degraded  3    3         2       github\n"
        );
```

`crates/balerix/src/commands/plugin.rs`, in `renders_the_plugin_table_and_sync_report`, after the two `render_sync` assertions:

```rust
        // Spec L-6: the fleets a removed plugin owned, one line each
        let r = SyncReport {
            stopped: vec!["gh".into()],
            downed: vec!["gh-acme-api".into(), "gh-acme-web".into()],
            down_failed: vec!["gh-acme-old: fleet task is gone".into()],
            ..SyncReport::default()
        };
        assert_eq!(
            render_sync(&r),
            "stopped: gh\n\
             fleet gh-acme-api: down\n\
             fleet gh-acme-web: down\n\
             fleet gh-acme-old: fleet task is gone (down failed)\n"
        );
```

`crates/balerix/tests/cli_fleet.rs`: add `use std::collections::BTreeMap;` and `use balerix_server::{ApplyMode, Caller};`, and the test

```rust
/// Spec L §5: a fleet a plugin applied shows its owner, refuses the
/// CLI's `up`, `update` and `down`, and goes down with `--force`.
#[test]
fn a_managed_fleet_shows_its_owner_and_needs_force_to_go_down() {
    let s = stub();
    let home = s.home.path();
    let fleet = fleet_file(home);
    let fleet = fleet.to_str().unwrap();
    // seeded as the daemon's manage path would leave it: owned by `gh`
    let spec = balerix_api::FleetSpec {
        name: "payments".into(),
        crews: BTreeMap::from([(
            "backend".to_string(),
            balerix_api::CrewSpec {
                repo: "acme/payments-api".into(),
                git_ref: "main".into(),
                git: balerix_api::GitSettings::default(),
                agents: BTreeMap::from([(
                    "alice".to_string(),
                    balerix_api::AgentSettings::default(),
                )]),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    s._rt
        .block_on(s.daemon.apply_as(
            &"payments".parse().unwrap(),
            spec,
            Default::default(),
            ApplyMode::Create,
            &Caller::Plugin("gh".parse().unwrap()),
        ))
        .unwrap();

    balerix(home)
        .args(["status", "payments"])
        .assert()
        .success()
        .stdout(predicate::str::contains("  managed by gh\n"));
    balerix(home)
        .args(["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("MANAGED BY"))
        .stdout(predicate::str::contains("  gh\n"));
    let out = balerix(home)
        .args(["status", "payments", "--json"])
        .assert()
        .success();
    let rec: FleetRecord = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(rec.owner.as_deref(), Some("gh"));

    for cmd in [["up", fleet], ["update", fleet]] {
        balerix(home)
            .args(cmd)
            .args(["--no-host-defaults", "--no-wait"])
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "fleet payments is managed by plugin gh (HTTP 409)",
            ));
    }
    balerix(home)
        .args(["down", "payments", "--timeout", "30s"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "fleet payments is managed by plugin gh (HTTP 409)",
        ));
    balerix(home)
        .args(["down", "payments", "--force", "--timeout", "30s"])
        .assert()
        .success()
        .stdout(predicate::str::contains("payments  down  generation 1 (observed 1)  managed by gh"));
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `mise x -- cargo nextest run -p balerix`
Expected: the two unit tests fail on the rendered strings; the binary test fails at `status` (no `managed by`), and `--force` is an unknown flag.

- [ ] **Step 3: Implement**

`crates/balerix/src/cli.rs`, `DownArgs`, after `purge`:

```rust
    /// Down a fleet a plugin manages (Spec L §5). Without it a managed
    /// fleet is refused; with it the plugin's next apply resumes it.
    #[arg(long)]
    pub force: bool,
```

`crates/balerix/src/commands/fleet.rs`:

```rust
pub fn render_status(r: &FleetRecord) -> String {
    let mut out = format!(
        "{}  {}  generation {} (observed {})",
        r.name(),
        label(r.status.phase),
        r.generation,
        r.status.observed_generation
    );
    if let Some(plugin) = &r.owner {
        out.push_str(&format!("  managed by {plugin}"));
    }
    out.push('\n');
    let rows: Vec<[String; 5]> = r
        .status
        .agents
        .iter()
        .map(|(id, a)| {
            [
                id.clone(),
                label(a.phase),
                a.restarts.to_string(),
                plugins_cell(a),
                a.message.clone(),
            ]
        })
        .collect();
    out.push_str(&table(
        &["AGENT", "PHASE", "RESTARTS", "PLUGINS", "MESSAGE"],
        &rows,
    ));
    out
}

pub fn render_list(rows: &[FleetSummary]) -> String {
    if rows.is_empty() {
        return "no fleets\n".to_string();
    }
    let rows: Vec<[String; 6]> = rows
        .iter()
        .map(|s| {
            [
                s.name.clone(),
                label(s.phase),
                s.generation.to_string(),
                s.observed_generation.to_string(),
                s.agents.to_string(),
                s.managed_by.clone().unwrap_or_else(|| "-".to_string()),
            ]
        })
        .collect();
    table(
        &["NAME", "PHASE", "GEN", "OBSERVED", "AGENTS", "MANAGED BY"],
        &rows,
    )
}
```

and in `down_command` the query becomes

```rust
    let q = DownQuery {
        keep_repos: args.keep || args.keep_repos,
        keep_sessions: args.keep || args.keep_sessions,
        purge: args.purge,
        force: args.force,
    };
```

`crates/balerix/src/commands/plugin.rs`: import `balerix_api::DownQuery`;

```rust
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
    // Spec L-6: the fleets a removed plugin owned, one line each
    for fleet in &r.downed {
        out.push_str(&format!("fleet {fleet}: down\n"));
    }
    for failure in &r.down_failed {
        out.push_str(&format!("fleet {failure} (down failed)\n"));
    }
    if out.is_empty() {
        out.push_str("nothing to do\n");
    }
    out
}
```

and in `remove_command`, the `--purge` branch:

```rust
    if args.purge {
        let client = Client::connect(args.api_url.as_deref())
            .map_err(|e| anyhow!("{e}; --purge needs a running daemon (the entry was removed)"))?;
        let report = client.sync_plugins()?;
        let mut out = render_sync(&report);
        // Spec L-6: `--purge` purges the plugin's fleets too. The daemon
        // downed them during the sync; a second, forced down with `purge`
        // deletes their records and directories.
        for fleet in &report.downed {
            client.down(
                fleet,
                &DownQuery {
                    purge: true,
                    force: true,
                    ..DownQuery::default()
                },
            )?;
            out.push_str(&format!("fleet {fleet}: purged\n"));
        }
        client.purge_plugin(&args.name)?;
        return Ok(format!(
            "{out}removed {} and purged its state\n",
            args.name
        ));
    }
```

- [ ] **Step 4: Run the tests and the gate**

Run: `mise x -- cargo nextest run -p balerix` then `mise run check`
Expected: PASS. The existing `up_status_list_update_and_down_through_the_binary` still passes: its `list` assertions are substring matches and the new column sits after them.

- [ ] **Step 5: Commit**

```bash
git add crates/balerix
git commit -m "feat(cli): show a fleet's managing plugin, down --force, and the fleets plugin remove took down (Spec L §5)"
```

---

### Task 9: The fake plugin's manage mode and the e2e journey (`balerix dev fake-plugin`, `e2e.rs`)

**Files:**
- Modify: `crates/balerix/src/commands/dev.rs:292-326`, `crates/balerix/tests/e2e.rs` (a `status_of` helper, a `managed_fleet_file` helper, one new journey)

**Interfaces:**
- Consumes: `Host::apply_fleet` (Task 7); the plugin's `plugins.yaml` `config` (delivered in the `hello` reply).
- Produces: with `config.manage = { "fleet": <name>, "file": <fleet file object> }` in its `plugins.yaml` entry, `dev fake-plugin` applies that fleet after `hello` and writes the outcome (the `FleetRecord` as JSON, or `{ "error": … }`) to `scratch/fake-plugin.manage`; `manage_from_config(host, config, scratch)` is the testable half.

- [ ] **Step 1: Write the failing unit test**

`crates/balerix/src/commands/dev.rs` `mod tests`:

```rust
    /// Spec L: `config.manage` in the fake's `plugins.yaml` entry is what
    /// the e2e uses to make a plugin apply a fleet; the outcome lands in
    /// scratch for the journey to read.
    #[test]
    fn the_fake_plugin_applies_the_fleet_its_config_names() {
        use balerix_plugin_sdk::testing::FakeHost;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let fake = FakeHost::start("tok", json!({}), vec![]).await;
            let host = balerix_plugin_sdk::Host::new(fake.env("fake", Path::new("/s"))).unwrap();
            let scratch = tempfile::tempdir().unwrap();
            // no `manage`: nothing happens, no file
            manage_from_config(&host, &json!({ "greeting": "hi" }), scratch.path())
                .await
                .unwrap();
            assert!(!scratch.path().join("fake-plugin.manage").exists());
            assert!(fake.applied_fleets().is_empty());
            // `manage`: the file is applied and the record recorded
            let file = json!({ "apiVersion": "balerix/v1", "kind": "Fleet", "crews": {} });
            manage_from_config(
                &host,
                &json!({ "manage": { "fleet": "managed", "file": file } }),
                scratch.path(),
            )
            .await
            .unwrap();
            let outcome: Value = serde_json::from_str(
                &std::fs::read_to_string(scratch.path().join("fake-plugin.manage")).unwrap(),
            )
            .unwrap();
            assert_eq!(outcome["spec"]["name"], "managed");
            assert_eq!(outcome["owner"], "plugin");
            assert_eq!(fake.applied_fleets(), vec![("managed".to_string(), file)]);
            // a refusal is recorded too, not fatal
            fake.fail_manage(Some((400, "name: \"x\" does not match the fleet managed")));
            manage_from_config(
                &host,
                &json!({ "manage": { "fleet": "managed", "file": {} } }),
                scratch.path(),
            )
            .await
            .unwrap();
            let outcome: Value = serde_json::from_str(
                &std::fs::read_to_string(scratch.path().join("fake-plugin.manage")).unwrap(),
            )
            .unwrap();
            assert_eq!(
                outcome["error"],
                "daemon: HTTP 400: name: \"x\" does not match the fleet managed"
            );
        });
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `mise x -- cargo nextest run -p balerix the_fake_plugin_applies`
Expected: compile error — no `manage_from_config`.

- [ ] **Step 3: Implement the manage mode**

`crates/balerix/src/commands/dev.rs`: in `fake_plugin_command`, after the `eprintln!("fake-plugin: hello acknowledged…")` line and before `server.await??;`:

```rust
        manage_from_config(&host, &resp.config, &scratch).await?;
```

and add the function (with `use balerix_plugin_sdk::Host;` at the top of the file):

```rust
/// Spec L: `config.manage = { fleet, file }` in the plugin's `plugins.yaml`
/// entry makes the fake apply that fleet file once it is up — how the e2e
/// exercises a plugin-managed fleet. The outcome, the record or
/// `{ "error" }`, lands in `scratch/fake-plugin.manage`; a refusal is not
/// fatal, the journey reads it.
async fn manage_from_config(host: &Host, config: &Value, scratch: &Path) -> Result<()> {
    let Some(manage) = config.get("manage") else {
        return Ok(());
    };
    let outcome = match (manage["fleet"].as_str(), manage.get("file")) {
        (Some(fleet), Some(file)) => match host.apply_fleet(fleet, file).await {
            Ok(record) => serde_json::to_value(record)?,
            Err(e) => json!({ "error": e.to_string() }),
        },
        _ => json!({ "error": "config.manage needs `fleet` and `file`" }),
    };
    std::fs::write(
        scratch.join("fake-plugin.manage"),
        serde_json::to_string_pretty(&outcome)?,
    )?;
    eprintln!("fake-plugin: manage outcome written");
    Ok(())
}
```

Run: `mise x -- cargo nextest run -p balerix the_fake_plugin_applies`
Expected: PASS.

- [ ] **Step 4: Write the e2e journey**

`crates/balerix/tests/e2e.rs`. In `impl World`, beside `status`:

```rust
    fn status_of(&self, fleet: &str) -> FleetRecord {
        serde_json::from_str(&self.ok(&["status", fleet, "--json"])).unwrap()
    }
```

Beside `fleet_yaml`:

```rust
/// The fleet a managed-fleet journey's plugin applies: `fleet_yaml`'s
/// shape as the JSON object `PUT fleets/{name}` takes, one agent, on
/// fake-claude, under a real nono profile.
fn managed_fleet_file(bare: &Path) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "balerix/v1", "kind": "Fleet", "name": "managed",
        "defaults": {
            "claude": {
                "binary": BALERIX,
                "args": ["dev", "fake-claude", "--verbose"],
                "settings": { "model": "sonnet" }
            },
            "sandbox": { "network": { "block": false } },
            "tools": {}
        },
        "crews": {
            "c": {
                "repo": format!("file://{}", bare.display()),
                "ref": "main",
                "git": { "push": false, "auth": "none" },
                "agents": { "alice": {} }
            }
        }
    })
}
```

The journey, after `plugin_protocol_journey`:

```rust
/// Spec L, end to end: a plugin with `manage` applies a fleet from its
/// config at hello; the fleet runs under the plugin's name; the CLI is
/// refused; removing the plugin takes the fleet down; `--force` purges it.
#[test]
fn plugin_manage_journey() {
    let Some(nono) = tool("nono") else {
        assert!(!require_or_skip("nono", false));
        return;
    };
    for t in ["git", "gh", "mise", "tmux"] {
        if !require_or_skip(t, tool(t).is_some()) {
            return;
        }
    }
    reap_earlier_runs();
    let root = TempRoot::new(Path::new(env!("CARGO_TARGET_TMPDIR")), "e2e-manage");
    if !require_or_skip("landlock", landlock_works(&nono, &root)) {
        return;
    }
    let w = World {
        home: root.join("home"),
        socket: format!("balerix-e2e-manage-{}", std::process::id()),
        tmux: tool("tmux").unwrap(),
    };
    fs::create_dir_all(&w.home).unwrap();
    let cfg = w.home.join(".config/balerix");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("mise.toml"), "[tools]\n").unwrap();

    // the same bare repo recipe as the first journey
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    fs::write(work.join("README"), "hi\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("repo.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );

    // the package: `fake`, with `manage`, told which fleet to apply
    let pkg = root.join("fake-pkg");
    plugin_package(&pkg, "needs: [fleets, manage]\n");
    let manifest = fs::read_to_string(pkg.join("balerix-plugin.yaml"))
        .unwrap()
        .replace("name: hello", "name: fake");
    fs::write(pkg.join("balerix-plugin.yaml"), manifest).unwrap();
    let config = serde_json::json!({
        "manage": { "fleet": "managed", "file": managed_fleet_file(&bare) }
    });
    // JSON is YAML: the object rides on one line after `config:`
    fs::write(
        cfg.join("plugins.yaml"),
        format!(
            "plugins:\n  - name: fake\n    source: \"{}\"\n    config: {}\n",
            pkg.display(),
            config
        ),
    )
    .unwrap();

    let out = w.ok(&[
        "serve",
        "-d",
        "--bind",
        "127.0.0.1:0",
        "--tmux-socket",
        &w.socket,
    ]);
    assert!(out.contains("http://127.0.0.1:"), "{out}");
    let plugin_dir = w.state().join("plugins/fake");
    wait_plugin_ready(&w, "fake", &plugin_dir);

    // the plugin applied its fleet at hello, and owns it
    let outcome = wait_file(&plugin_dir.join("scratch/fake-plugin.manage"));
    let outcome: serde_json::Value = serde_json::from_str(&outcome).unwrap();
    assert!(outcome.get("error").is_none(), "{outcome}");
    assert_eq!(outcome["owner"], "fake", "{outcome}");
    assert_eq!(outcome["generation"], 1, "{outcome}");
    let status = w.ok(&["status", "managed"]);
    assert!(status.contains("  managed by fake"), "{status}");
    let list = w.ok(&["list"]);
    assert!(
        list.lines().any(|l| l.starts_with("managed ") && l.ends_with("fake")),
        "{list}"
    );

    // it becomes ready like any fleet: the relay's SessionStart from
    // fake-claude, through a real nono profile
    let start = Instant::now();
    loop {
        let rec = w.status_of("managed");
        if rec.status.phase == balerix_api::FleetPhase::Ready {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(180),
            "managed fleet never ready: {rec:?}"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(
        w.state()
            .join("fleets/managed/crews/c/agents/alice/workspace/README")
            .exists(),
        "the agent's worktree was created from the file's repo"
    );

    // the CLI may not take it over
    let fleet = root.join("managed.yaml");
    fs::write(
        &fleet,
        fleet_yaml(&bare, None, None).replace("name: e2e", "name: managed"),
    )
    .unwrap();
    let out = w.run(&[
        "up",
        &fleet.display().to_string(),
        "--no-host-defaults",
        "--no-wait",
    ]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("fleet managed is managed by plugin fake"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = w.run(&["down", "managed", "--timeout", "30s"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("fleet managed is managed by plugin fake"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // removing the plugin takes its fleet down; --force purges what is left
    let out = w.ok(&["plugin", "remove", "fake"]);
    assert!(out.contains("stopped: fake"), "{out}");
    assert!(out.contains("fleet managed: down"), "{out}");
    let start = Instant::now();
    loop {
        let rec = w.status_of("managed");
        if rec.status.phase == balerix_api::FleetPhase::Down {
            assert_eq!(rec.owner.as_deref(), Some("fake"), "the owner survives the down");
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "managed fleet never down: {rec:?}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    let out = w.ok(&["down", "managed", "--force", "--purge", "--timeout", "60s"]);
    assert!(out.contains("managed: purged"), "{out}");
    assert_eq!(w.ok(&["list"]), "no fleets\n");
    drop(w);
}
```

- [ ] **Step 5: Run the e2e, then everything**

Run: `mise run e2e` (needs git, gh, mise, nono, tmux and Landlock; `BALERIX_REQUIRE_TOOLS=1` is set by the task so a missing tool fails instead of skipping). Then `mise run check` and `mise run test-it`.
Expected: PASS. The three existing journeys are unchanged. If `wait_plugin_ready` times out, read `plugins/fake/logs/nono.log` as its message says: the fake's `hello` and its `apply_fleet` both run under the plugin's nono profile, and the daemon URL is an `open_port`.

- [ ] **Step 6: Commit**

```bash
git add crates/balerix
git commit -m "test(e2e): a plugin-managed fleet end to end through dev fake-plugin's manage mode (Spec L §8, §10)"
```

---

### Task 10: Documentation and the final gate

**Files:**
- Modify: `docs/THREAT-MODEL.md` (line 23, the accepted risks after line 79, the mitigation table after line 104), `ARCHITECTURE.md` (lines 22-27, after line 169, the decisions list), `AGENTS.md` (Gotchas), `examples/payments.yaml`, `docs/superpowers/specs/2026-09-22-balerix-l-managed-fleets-design.md` (a new §12), `mise.toml`/`.github/workflows` only if `mise run plugins` is not already in CI for an SDK change (it is; nothing to do)

- [ ] **Step 1: The threat model**

`docs/THREAT-MODEL.md`:

- Line 23, the plugin ↔ daemon boundary: the host routes list gains `` `fleets/{name}` (`PUT`/`DELETE`, `manage`) ``.
- After the `workspace` bullet (line 79) under **Out of scope / accepted risks**, the two bullets Spec L §7 gives, verbatim:

```
- **A plugin with `manage` runs agents with the operator's credentials.** It can apply any fleet file, so any repository the operator's gh token reaches and any settings the resolver accepts, and down what it created. The operator's choice in `needs`, like `attach` and `workspace`; bounded by the owner rule (it cannot touch a fleet it did not create, and the CLI cannot silently take over its fleets), by the same validation `up` applies, and by every agent still running inside its nono profile. The plugin never sees the credentials: the route takes a file, the daemon reads the bundle (`crates/balerix-server/src/daemon.rs::manage_fleet`).
- **The fleet file crosses the plugin → daemon boundary as untrusted input** with the same treatment as the CLI's: full validation through the resolver, `deny_unknown_fields`, name rules, exact tool versions. `branch` is validated as a ref name before it reaches a git argv (`git worktree add -b <branch> <path> origin/<branch>`, `crates/balerix-api/src/branch.rs`); the leading-`-` refusal is what keeps a name from being read as a flag.
```

- The mitigation table, after the "Plugin host routes" row:

```
| A plugin's fleet file | `manage` gates `PUT`/`DELETE fleets/{name}` (403); `name` must equal the path's, `balerix`/`watch` refused; the file resolves through the same `balerix-config` resolver as `up` (400 with the path); a plugin touches only fleets it owns and the CLI needs `--force` for one it does not (409); credentials are read by the daemon from its own host home, never sent by the plugin | `plugin_api.rs::{put_fleet, delete_fleet}`, `daemon.rs::{manage_fleet, check_owner}`, `balerix/src/wiring.rs::HostResolver` |
```

- [ ] **Step 2: The architecture note**

`ARCHITECTURE.md`:

- Lines 22-27, the `balerix-core` bullet: the port list becomes `` `Materializer`, `AgentRunner`, `Clock`, `WorkspaceReader`, `FleetResolver`, `CredentialSource` today; `FleetStore` and `EventHandler` arrive with the server ``; add one sentence: `FleetResolver` and `CredentialSource` (Spec L) are implemented by the binary over `balerix-config`, the one crate that resolves a fleet file, so the daemon resolves a plugin's file without depending on it.
- After the **Workspace reads and review (Spec C)** paragraph (line 169), a new paragraph:

```
**Plugin-managed fleets (Spec L):** a plugin declaring `manage` applies
a fleet from an unresolved fleet file — `PUT
/v1/plugin-host/fleets/{name}` takes what `balerix up -f` reads, as JSON
— and the daemon does what `up` does: resolves it through the
`FleetResolver` port, reads the operator's credentials through
`CredentialSource`, and applies. The record carries the plugin as its
`owner`; the admin routes refuse an owned fleet (409) except `down
--force`, and a plugin may only apply or down what it owns. `plugin
remove` downs the fleets the plugin owned. An agent's `branch` setting
makes an existing remote branch its worktree branch and start point
(a PR's head); the diff base stays the crew's `ref`.
```

- The decisions list: after the `balerix` reserved-name bullet (line 244), add `- **A fleet has at most one writer.** A record's `owner` is set by the first plugin apply and never transferred; the CLI's override is `down --force`, which keeps the owner so the plugin's next apply resumes the fleet (Spec L §5).`

- [ ] **Step 3: The gotchas**

`AGENTS.md`, **Gotchas**, after the `plugins.yaml … secrets` bullets:

```
- A fleet record's `owner` (Spec L) is set by the first `PUT
  /v1/plugin-host/fleets/{name}` and never transferred: `up`/`update`
  on it are 409 `fleet <f> is managed by plugin <p>`, `down` needs
  `--force`, and a forced down keeps the owner so the plugin's next
  apply resumes it. `plugin remove` downs the plugin's fleets during the
  sync (`SyncReport.downed`); a plugin that was already undeclared when
  the daemon started downs nothing — `down --force` by hand. The
  daemon-side rule is `Daemon::check_owner`; `apply`/`down` are the
  admin wrappers of `apply_as`/`down_as`.
- The resolver a plugin's fleet file goes through lives in the binary
  (`crates/balerix/src/wiring.rs::HostResolver`, both ports): it reads
  `HostPaths::discover()` at call time, so the daemon's `HOME` is the
  operator whose credentials every managed fleet gets. `balerix-server`
  sees only the `FleetResolver`/`CredentialSource` ports;
  `Harness` answers them with `FakeResolver` (no answer → `name: no
  resolver answer configured`; set it per test) and `FakeCredentials`.
- `AgentSettings.branch` makes the *remote* branch the worktree branch
  and its start point (`ResolvedAgent::start_ref`); a branch the remote
  lacks fails the materialize step with git's message and retries at the
  resync cadence. The workspace diff base is still `origin/<crew ref>`.
- `dev fake-plugin` applies a fleet when its `plugins.yaml` entry has
  `config: { manage: { fleet, file } }` (the e2e's managed journey) and
  writes the outcome to `scratch/fake-plugin.manage`. The SDK's
  `FakeHost` answers `PUT fleets/{name}` with the record it already holds
  under that name (`set_fleets`), else a fresh one owned by `plugin`.
```

- [ ] **Step 4: The example and the spec**

`examples/payments.yaml`: under one agent add a commented line `# branch: feature/issue-12   # work on an existing remote branch instead of balerix/<fleet>/<crew>/<agent> (Spec L)`.

The Spec L file gains, at the end:

```
## 12. Recorded at implementation (2026-09-23)

- `Daemon::apply`/`down` stay the admin wrappers; the shared forms are
  `apply_as(name, spec, credentials, ApplyMode, &Caller)` and
  `down_as(name, keep, purge, &Caller)`, with `ApplyMode::Upsert` for
  the plugin's `PUT` (§3.1 step 5 said `replace = true`; an absent name
  must create). `Caller::Admin { force }` / `Caller::Plugin(name)`.
- The owner error is `DaemonError::Managed(String)` → 409; a record
  is created with `FleetRecord::with_owner`.
- `plugin remove` downs owned fleets inside `Daemon::sync_plugins`, for
  every plugin the sync stopped; the fleets and failures ride back in
  `SyncReport.downed` / `SyncReport.down_failed` and the CLI prints one
  line per fleet from them. `--purge` re-downs each with `purge` +
  `force` from the CLI. A plugin already undeclared at daemon start
  downs nothing.
- `balerix_config::from_value` reads the file from JSON with
  `serde_path_to_error`, so a shape error names its key (`file` for the
  root); the daemon checks `file.name` against the path before resolving.
- The plugin `DELETE` accepts `force` and ignores it.
- `dev fake-plugin`'s manage mode is driven by `config.manage` in
  `plugins.yaml`, not a flag: the package's `start` task takes no args.
- `FakeHost` records refused calls too (`applied_fleets`, `downed_fleets`)
  and fails both routes with `fail_manage(Some((status, message)))`.
- `check_branch_name` also refuses the bare `@`, as git does.
```

- [ ] **Step 5: The final gate**

Run, in this order, and paste each result into the PR description:

```bash
mise run check
mise run test-it
mise run e2e
mise run plugins      # the SDK changed; every standalone plugin project must still build and test
scripts/check-core-deps.sh
```

Expected: all PASS. `mise run plugins` compiles `flow`, `web`, `matrix` and `common` against the changed SDK; nothing in them calls the new methods, so no source change is expected there — if one fails to compile, the SDK change was not additive and must be made so.

- [ ] **Step 6: Commit and open the PR**

```bash
git add docs/THREAT-MODEL.md ARCHITECTURE.md AGENTS.md examples/payments.yaml docs/superpowers/specs/2026-09-22-balerix-l-managed-fleets-design.md
git commit -m "docs: the manage capability, the owner rule, the two ports and the agent branch (Spec L §7, §10)"
```

PR title: `feat(core): plugin-managed fleets, owned records and a per-agent branch (Spec L)`. Body: the spec path, the ten commits, the four gate results, and the one new dependency with its reason.

---

## Self-review

**Spec coverage.** §3 `manage` → Task 1; §3.1 route and its five steps → Tasks 5 (steps 1–5 in `manage_fleet`) and 6 (the route, the body shape); §3.2 → Task 6; §3.3 SDK, `FakeHost`, fixtures, `docs/plugin-protocol.md` → Task 7; §4 ports, binary implementation, fakes, `FleetFile: Serialize` → Tasks 3, 5, 2; §5 owner on the record and summary, `list`/`status`, the five rules → Tasks 1, 5, 6, 8 (`plugin remove` → Task 5's `sync_plugins` and Task 8's output); §6 `branch` field, validation, `branch()`/start point, diff base unchanged, a missing remote branch → Tasks 1, 2, 3, 4; §7 threat model → Task 10; §8 every listed test has a task (`api`: Task 1; `config`: Task 2; `core`: Task 3; `workspace_it`: Task 4; `api_it`/`events_it` items: Task 5 unit tests and Task 6's `manage_it`; `cli_fleet`: Task 8; conformance: Task 7; e2e: Task 9); §10 done-when 1–5 → Tasks 9, 7, 9, 4, 10.

**Placeholders.** The `fleet-delete.json` `spec` is written as "copy from `fleet-put.json`" on purpose — the two files must be equal there, and the conformance test enforces it through the `FleetRecord` round trip.

**Type consistency.** `FleetRecord::with_owner(spec, Option<String>)` (Tasks 1, 5, 7); `Caller::Plugin(AgentName)` and `Caller::Admin { force }` (Tasks 5, 6, 8); `ApplyMode::{Create, Replace, Upsert}` (Tasks 5, 8); `apply_as(&FleetName, FleetSpec, CredentialBundle, ApplyMode, &Caller)` and `down_as(&FleetName, Keep, bool, &Caller)` (Tasks 5, 6, 8); `manage_fleet(&AgentName, &FleetName, Value)` (Tasks 5, 6); `FakeResolver::set(Result<FleetSpec, String>)` and `calls() -> Vec<(String, Value)>` (Tasks 3, 5, 6); `FakeCredentials::set(Result<CredentialBundle, String>)`, `calls() -> usize` (Tasks 3, 5); `Host::apply_fleet(&str, &Value)`, `down_fleet(&str, &DownQuery)` (Tasks 7, 9); `FakeHost::{applied_fleets, downed_fleets, fail_manage}` (Tasks 7, 9); `SyncReport.{downed, down_failed}` (Tasks 1, 5, 8); `DownQuery.force` and `to_query_string` (Tasks 1, 6, 7, 8); `ResolvedAgent::start_ref() -> &str` (Tasks 3, 4); `from_value(&Value)` (Tasks 2, 5); `world_with(&[(&str, &str)])` (Task 6).

**Review focus.** Each of the five lines names its test and task; all five tests are written out above.
