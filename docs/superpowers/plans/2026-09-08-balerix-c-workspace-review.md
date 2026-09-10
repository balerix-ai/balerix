# Balerix Spec C — Workspace Reads and Browser Code Review Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A fifth plugin capability, `workspace`, through which a plugin reads an agent's worktree via the daemon (diff against the crew's base, one file, one listing); and, on it, a review page in `balerix-plugin-web` with line comments, a collapsible column of the agent's hook events, and a submit that pastes the review into the agent as one message.

**Architecture:** Nine tasks, each mergeable. Wire types and the pure path rule land in `balerix-api`; a `WorkspaceReader` port and its fake in `balerix-core`; `balerix-runtime` implements the port over `git` with the repository's config escape hatches closed; the daemon adds three host routes gated by `Capability::Workspace` and an active pair; the SDK gets three `Host` methods, `FakeHost` state and three fixtures; `TmuxRunner::send_text` learns to paste multi-line text; then the web plugin gains an event buffer fed by `observe`, the review page, and the submission that renders one message and sends it through `send_text`. The e2e and the docs close it.

**Tech Stack:** Rust 1.98.1 (edition 2024); tokio 1.53.1, axum 0.8.9 (`ws`), reqwest 0.13.4 (no TLS), prometheus 0.14.0, serde/serde_json, thiserror, proptest 1.11.0; **no new workspace dependencies** (the web crate gains a dev-dependency on the already-pinned `proptest`); real `git 2.47`, `tmux 3.7c`, `nono 0.75.0`, `mise` for the integration and e2e tests. No new vendored JavaScript: the review page's diff parser and renderer are inline script.

**Spec:** `docs/superpowers/specs/2026-09-08-balerix-c-workspace-review-design.md` (the *Spec C*), on top of `docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md` (the *plugins spec*) §4, §5.1, §7, §18, and `docs/plugin-protocol.md`. Read Spec C whole before any task; §2 before Tasks 1, 3, 4; §3 before Tasks 2, 5; §4 before Tasks 6–8; §5–§8 before Task 9. Where this plan refines Spec C (recorded again in Task 9 as Spec C §12):

- **`check_path` lives in `balerix-api::workspace`**, not `balerix-core`. The web plugin validates comment paths with the same rule and may depend on `balerix-api` only; the function is pure (like `ResizeFrame::parse`, already in `balerix-api`), and the runtime, the fakes and the daemon all read it from there.
- **The submission body carries `base_ref` beside `head`**, so the message header can say `against origin/main` without a second diff call.
- **`Cmd` gains `run_with_exit_codes`**: `git diff --no-index` exits 1 when the files differ, which is the answer, not a failure.
- **`FakeHost::fail_actions`** makes the fake refuse `POST agents/…/actions` with a message, for the web plugin's 502 case.
- **A renamed file's per-file diff names both paths** (`diff … -- <old> <new>`); with the new path alone git would report an addition.
- **`old_path` is always serialized**, `null` when absent, as Spec C §2.2's example shows.

## Global Constraints

Copied from the specs and the earlier plans; every task's requirements include these.

- Rust **1.98.1**, `edition = "2024"`, `rust-version = "1.98"`; every tool in `mise.toml` is an exact version. Run cargo as `mise x -- cargo …` or through `mise run <task>`.
- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` pass at every commit. `unsafe_code = "forbid"`. No `unwrap`/`expect` outside tests (test modules and `tests/*.rs` carry `#![allow(clippy::unwrap_used, clippy::expect_used)]`). `std::env::set_var` is unsafe in edition 2024 — inject environment through parameters.
- Library crates return `thiserror` errors whose `Display` starts with the config path or the subject; only binaries use `anyhow`.
- Dependency direction: `api` leaf → `core` → `config` / `runtime` / `server` / `plugin-sdk` → binary; plugin crates depend on `balerix-plugin-sdk` and `balerix-api` only, never on `core`, `server`, `runtime` or `config`. `balerix-server` never imports `balerix-runtime`. Ports live in `balerix-core`; only the `balerix` binary wires adapters to ports.
- **No new workspace dependencies in this plan.** `sha2`, `hex`, `serde_path_to_error`, `proptest` are already in `[workspace.dependencies]`; the one `Cargo.toml` change is `proptest` as a dev-dependency of `balerix-plugin-web` (Task 6).
- Plain HTTP on `127.0.0.1` only. Every `reqwest::Client` is built with `.no_proxy()`.
- Secrets never appear in `Debug` output, logs, argv or the environment. The daemon logs plugin names, agent ids and error messages only; a `WorkspaceError::Io` names a daemon-side path and is logged, never sent to the plugin.
- Untrusted input: hook payloads, plugin bodies, browser requests and plugin configs are validated before use; proxied request bodies are capped at 1 MiB; every value the web pages render goes through `html_escape` (server side) or `textContent` (browser side).
- The daemon runs `git` only with `-C <workspace>`, the five `GIT_*` variables scrubbed, `GIT_OPTIONAL_LOCKS=0`, `-c core.fsmonitor=false`, `-c core.hooksPath=<empty dir>`, and `--no-ext-diff --no-textconv --no-color` on every `diff`; never `fetch`, never a credential helper (Spec C §3.2).
- Integration and e2e tests skip with a printed reason when a tool or Landlock is missing; `BALERIX_REQUIRE_TOOLS=1` (CI, `mise run test-it`, `mise run e2e`) panics instead. Temp roots live under `target/tmp`, never `/tmp`.
- Commit messages: imperative subject, body explains why, and end with the trailer line `Claude-Session: https://claude.ai/code/session_01XVWfZKBMR5cciaB4H5cXz5`.
- The pre-commit hook runs `mise run precommit` (gitleaks + `check`, e2e included) and prints thousands of lines: commit as `git commit -q -F - > "$SCRATCH/commit.log" 2>&1 <<'EOF' … EOF`, then `grep -E "Summary|FAIL" "$SCRATCH/commit.log"` and `git log --oneline -1`. The branch is `spec-c-workspace-review`, already holding the Spec C commit.

---

## File structure

```
crates/balerix-api/src/
  workspace.rs                     new: check_path, limits, WorkspaceDiff, FileDiff, FileStatus, WorkspaceTree, TreeEntry, EntryKind
  plugin.rs                        Capability::Workspace
  lib.rs                           re-exports
crates/balerix-core/src/
  ports.rs                         + WorkspaceError, WorkspaceReader
  fakes.rs                         + FakeWorkspace
  lib.rs                           re-exports
crates/balerix-runtime/src/
  inspect.rs                       new: impl WorkspaceReader for Runtime; parse_name_status, shape_patch
  tools.rs                         Cmd::run_with_exit_codes
  tmux.rs                          send_text pastes multi-line text
  lib.rs                           mod inspect
crates/balerix-runtime/tests/
  inspect_it.rs                    new: real git
  tmux_it.rs                       + multi-line send_text
crates/balerix-server/src/
  actor.rs                         Ports.workspace
  daemon.rs                        workspace(), base_ref()
  plugin_api.rs                    + three workspace routes
  api.rs                           From<WorkspaceError> for ApiError
  plugins/host.rs                  Ports clone carries workspace
  testing.rs                       Harness.workspace
crates/balerix-server/tests/
  support/mod.rs                   web package declares workspace
  workspace_it.rs                  new
  plugins_it.rs                    Ports literal gains workspace
crates/balerix/src/commands/serve.rs Runtime wired as the WorkspaceReader
crates/balerix-plugin-sdk/src/
  host.rs                          workspace_diff, workspace_file, workspace_tree
  testing.rs                       FakeHost workspaces + fail_actions; Harness::post_route
crates/balerix-plugin-sdk/tests/conformance.rs  three fixtures; 21
docs/plugin-protocol/workspace-{diff,file,tree}.json  new
docs/plugin-protocol.md            §3 rows, Workspace paragraph, §6 count
crates/balerix-plugin-web/
  package/balerix-plugin.yaml      observe all nine; needs + actions, workspace
  src/state.rs                     + Entry, event buffers, summarize, cut_payload, phase_of
  src/plugin.rs                    observe; three new metrics
  src/routes.rs                    + review page, diff.json, file, events.json, POST review; index link
  src/review.rs                    new: ReviewBody, Comment, Side, validate, render_message
  src/lib.rs                       mod review
  tests/plugin_it.rs               + events, review page, submission
crates/balerix/tests/e2e.rs        web_journey: diff.json, review, events; raw_post
scripts/verify-claude.sh           the review step in the browser instructions
docs/THREAT-MODEL.md, ARCHITECTURE.md, AGENTS.md, README.md, Spec C §12
```

---

### Task 1: Wire types, the path rule, the port and its fake

**Files:**
- Create: `crates/balerix-api/src/workspace.rs`
- Modify: `crates/balerix-api/src/plugin.rs:49-57` (`Capability`), `crates/balerix-api/src/lib.rs`
- Modify: `crates/balerix-core/src/ports.rs` (after `RunnerError`), `crates/balerix-core/src/fakes.rs`, `crates/balerix-core/src/lib.rs`

**Interfaces:**
- Consumes: `balerix_core::ports::first_line`, `fakes::Recorder`, `fakes::lock`.
- Produces:
  ```rust
  // balerix_api (workspace.rs, re-exported at the crate root)
  pub const WORKSPACE_FILE_LIMIT: u64 = 1 << 20;          // bytes, `file`
  pub const WORKSPACE_PATCH_LIMIT: usize = 256 << 10;      // bytes, one patch
  pub const WORKSPACE_FILE_COUNT_LIMIT: usize = 500;       // files in a diff
  pub fn check_path(path: &str) -> Result<(), String>;     // Err(reason)
  pub enum FileStatus { Added, Modified, Deleted, Renamed, Copied, Typechange }   // lowercase on the wire
  pub struct FileDiff { pub path: String, pub old_path: Option<String>, pub status: FileStatus,
                        pub uncommitted: bool, pub binary: bool, pub patch: String, pub truncated: bool }
  pub struct WorkspaceDiff { pub base_ref: String, pub merge_base: String, pub head: String,
                             pub files: Vec<FileDiff>, pub truncated: bool }
  pub enum EntryKind { File, Dir, Symlink, Other }         // lowercase on the wire
  pub struct TreeEntry { pub name: String, pub kind: EntryKind, pub size: Option<u64> }  // size omitted when None
  pub struct WorkspaceTree { pub path: String, pub entries: Vec<TreeEntry> }
  pub enum Capability { Fleets, Actions, Attach, Kv, Workspace }
  // balerix_core (ports.rs, re-exported)
  pub enum WorkspaceError { Missing(String), NoSuchPath, InvalidPath(String), NotAFile, NotADirectory,
                            TooLarge { limit: u64 }, Tool { id, subcommand, args, stderr }, Io { path, message } }
  pub trait WorkspaceReader: Send + Sync {
      fn diff(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceDiff, WorkspaceError>;
      fn read_file(&self, agent: &AgentId, path: &str) -> Result<Vec<u8>, WorkspaceError>;
      fn list_dir(&self, agent: &AgentId, path: &str) -> Result<WorkspaceTree, WorkspaceError>;
  }
  // balerix_core::fakes
  pub struct FakeWorkspace;   // Default
  impl FakeWorkspace { pub fn set(&self, agent: &AgentId, diff: WorkspaceDiff, files: BTreeMap<String, Vec<u8>>);
                       pub fn calls(&self) -> Vec<String> }
  ```

- [ ] **Step 1: Write the failing api tests**

Create `crates/balerix-api/src/workspace.rs` with only the test module for now:

```rust
//! Workspace wire types (Spec C §2.2) and the path rule every reader
//! applies before any I/O (Spec C §2.2 "Paths"). The rule is here, not in
//! `balerix-core`, because the web plugin validates comment paths with it
//! and may depend on this crate only.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_path_rule_accepts_relative_paths_and_names_the_reason_otherwise() {
        for ok in ["", "src", "src/lib.rs", "a-b_c.d/e~f", "dir/.gitignore", "gitx/.gith"] {
            assert_eq!(check_path(ok), Ok(()), "{ok:?}");
        }
        for (bad, reason) in [
            ("/etc/passwd", "absolute"),
            ("../x", "\"..\" segment"),
            ("a/../b", "\"..\" segment"),
            ("./a", "\".\" segment"),
            ("a//b", "empty segment"),
            ("a/", "empty segment"),
            (".git", ".git segment"),
            ("a/.git/config", ".git segment"),
            ("a/.GIT", ".git segment"),
            ("a\\b", "contains a backslash"),
            ("a\0b", "contains NUL"),
        ] {
            assert_eq!(check_path(bad), Err(reason.to_string()), "{bad:?}");
        }
        let long = "x".repeat(4097);
        assert_eq!(check_path(&long), Err("longer than 4096 bytes".to_string()));
    }

    #[test]
    fn the_diff_and_tree_round_trip_with_lowercase_enums() {
        let d = WorkspaceDiff {
            base_ref: "origin/main".into(),
            merge_base: "a".repeat(40),
            head: "b".repeat(40),
            files: vec![FileDiff {
                path: "COPYING".into(),
                old_path: Some("LICENSE".into()),
                status: FileStatus::Renamed,
                uncommitted: false,
                binary: false,
                patch: "diff --git a/LICENSE b/COPYING\n".into(),
                truncated: false,
            }],
            truncated: false,
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["files"][0]["status"], "renamed");
        assert_eq!(v["files"][0]["old_path"], "LICENSE");
        let back: WorkspaceDiff = serde_json::from_value(v).unwrap();
        assert_eq!(back, d);
        let plain: FileDiff = serde_json::from_value(json!({
            "path": "x", "status": "added", "uncommitted": true, "binary": false, "patch": ""
        }))
        .unwrap();
        assert_eq!(plain.old_path, None);
        assert!(!plain.truncated, "defaults");
        assert_eq!(
            serde_json::to_value(&plain).unwrap()["old_path"],
            serde_json::Value::Null,
            "always present, null when absent"
        );
        let t = WorkspaceTree {
            path: "src".into(),
            entries: vec![
                TreeEntry { name: "lib.rs".into(), kind: EntryKind::File, size: Some(13) },
                TreeEntry { name: "sub".into(), kind: EntryKind::Dir, size: None },
            ],
        };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["entries"][0]["kind"], "file");
        assert_eq!(v["entries"][0]["size"], 13);
        assert!(v["entries"][1].get("size").is_none(), "size only for files");
        assert_eq!(serde_json::from_value::<WorkspaceTree>(v).unwrap(), t);
        assert_eq!(WORKSPACE_FILE_LIMIT, 1 << 20);
        assert_eq!(WORKSPACE_PATCH_LIMIT, 256 << 10);
        assert_eq!(WORKSPACE_FILE_COUNT_LIMIT, 500);
    }
}
```

Add `pub mod workspace;` to `crates/balerix-api/src/lib.rs` and the re-export line:

```rust
pub use workspace::{
    EntryKind, FileDiff, FileStatus, TreeEntry, WORKSPACE_FILE_COUNT_LIMIT, WORKSPACE_FILE_LIMIT,
    WORKSPACE_PATCH_LIMIT, WorkspaceDiff, WorkspaceTree, check_path,
};
```

Run: `mise x -- cargo test -p balerix-api workspace`
Expected: compile error (nothing defined).

- [ ] **Step 2: The types and the rule**

Above the test module in `workspace.rs`:

```rust
use serde::{Deserialize, Serialize};

/// `GET …/workspace/file` refuses a file larger than this (413).
pub const WORKSPACE_FILE_LIMIT: u64 = 1 << 20;
/// One file's `patch` is cut at a line boundary beyond this.
pub const WORKSPACE_PATCH_LIMIT: usize = 256 << 10;
/// A diff lists at most this many files, in path order.
pub const WORKSPACE_FILE_COUNT_LIMIT: usize = 500;

/// The rule for `path` on the `file` and `tree` routes and for comment
/// paths: relative, `/`-separated, at most 4096 bytes, no empty, `.` or
/// `..` segment, no `\` or NUL, and no segment named `.git` (any case).
/// The empty path is the worktree root. `Err` is the reason, for
/// `workspace: invalid path: <reason>`.
pub fn check_path(path: &str) -> Result<(), String> {
    if path.len() > 4096 {
        return Err("longer than 4096 bytes".into());
    }
    if path.contains('\0') {
        return Err("contains NUL".into());
    }
    if path.contains('\\') {
        return Err("contains a backslash".into());
    }
    if path.starts_with('/') {
        return Err("absolute".into());
    }
    if path.is_empty() {
        return Ok(());
    }
    for segment in path.split('/') {
        match segment {
            "" => return Err("empty segment".into()),
            "." | ".." => return Err(format!("{segment:?} segment")),
            s if s.eq_ignore_ascii_case(".git") => return Err(".git segment".into()),
            _ => {}
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Typechange,
}

/// One file of a `WorkspaceDiff` (Spec C §2.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    /// Set for `renamed` and `copied`; serialized as `null` otherwise.
    #[serde(default)]
    pub old_path: Option<String>,
    pub status: FileStatus,
    /// The worktree differs from `HEAD` for this path (an untracked file
    /// is `added` and uncommitted).
    pub uncommitted: bool,
    /// `patch` is empty for a binary file.
    pub binary: bool,
    /// Unified diff for this one file, three lines of context, the
    /// `diff --git` header included.
    pub patch: String,
    /// The patch was cut at `WORKSPACE_PATCH_LIMIT`.
    #[serde(default)]
    pub truncated: bool,
}

/// `GET …/workspace/diff`: the worktree against the merge-base with
/// `base_ref`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDiff {
    /// `origin/<crew ref>`.
    pub base_ref: String,
    pub merge_base: String,
    pub head: String,
    pub files: Vec<FileDiff>,
    /// The file list stopped at `WORKSPACE_FILE_COUNT_LIMIT`.
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub kind: EntryKind,
    /// Files only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// `GET …/workspace/tree`: one directory, never recursive, `.git` never
/// listed, sorted by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceTree {
    pub path: String,
    pub entries: Vec<TreeEntry>,
}
```

In `crates/balerix-api/src/plugin.rs` add `Workspace,` after `Kv,` in `Capability`, and extend the existing `manifest_rejects_unknown_fields_and_capabilities` test with:

```rust
        assert_eq!(
            serde_json::from_value::<Capability>(json!("workspace")).unwrap(),
            Capability::Workspace
        );
```

Run: `mise x -- cargo test -p balerix-api`
Expected: PASS.

- [ ] **Step 3: Write the failing core tests**

In `crates/balerix-core/src/fakes.rs` tests (the `mod tests` at the bottom; it has `fn id(s) -> AgentId`), add:

```rust
    #[test]
    fn the_fake_workspace_answers_from_what_the_test_set() {
        use balerix_api::{EntryKind, FileDiff, FileStatus, WorkspaceDiff};
        let w = FakeWorkspace::default();
        assert_eq!(
            w.diff(&id("f/c/a"), "origin/main"),
            Err(WorkspaceError::Missing("f/c/a".into()))
        );
        let diff = WorkspaceDiff {
            base_ref: String::new(),
            merge_base: "m".into(),
            head: "h".into(),
            files: vec![FileDiff {
                path: "src/lib.rs".into(),
                old_path: None,
                status: FileStatus::Modified,
                uncommitted: true,
                binary: false,
                patch: "p".into(),
                truncated: false,
            }],
            truncated: false,
        };
        w.set(
            &id("f/c/a"),
            diff.clone(),
            BTreeMap::from([
                ("src/lib.rs".to_string(), b"fn a() {}\n".to_vec()),
                ("src/sub/x.rs".to_string(), b"x".to_vec()),
                ("README".to_string(), b"hi\n".to_vec()),
            ]),
        );
        let got = w.diff(&id("f/c/a"), "origin/main").unwrap();
        assert_eq!(got.base_ref, "origin/main", "the base the caller asked for");
        assert_eq!(got.files, diff.files);
        assert_eq!(
            w.read_file(&id("f/c/a"), "src/lib.rs").unwrap(),
            b"fn a() {}\n".to_vec()
        );
        assert_eq!(
            w.read_file(&id("f/c/a"), "nope"),
            Err(WorkspaceError::NoSuchPath)
        );
        assert_eq!(
            w.read_file(&id("f/c/a"), "src"),
            Err(WorkspaceError::NotAFile)
        );
        assert_eq!(
            w.read_file(&id("f/c/a"), "../x"),
            Err(WorkspaceError::InvalidPath("\"..\" segment".into()))
        );
        let big = vec![0u8; (balerix_api::WORKSPACE_FILE_LIMIT + 1) as usize];
        w.set(&id("f/c/b"), diff.clone(), BTreeMap::from([("big".to_string(), big)]));
        assert_eq!(
            w.read_file(&id("f/c/b"), "big"),
            Err(WorkspaceError::TooLarge {
                limit: balerix_api::WORKSPACE_FILE_LIMIT
            })
        );
        let root = w.list_dir(&id("f/c/a"), "").unwrap();
        assert_eq!(root.path, "");
        let names: Vec<(&str, EntryKind, Option<u64>)> = root
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.kind, e.size))
            .collect();
        assert_eq!(
            names,
            vec![
                ("README", EntryKind::File, Some(3)),
                ("src", EntryKind::Dir, None)
            ]
        );
        let src = w.list_dir(&id("f/c/a"), "src").unwrap();
        assert_eq!(src.entries.len(), 2);
        assert_eq!(src.entries[1].name, "sub");
        assert_eq!(
            w.list_dir(&id("f/c/a"), "README"),
            Err(WorkspaceError::NotADirectory)
        );
        assert_eq!(
            w.list_dir(&id("f/c/a"), "nope"),
            Err(WorkspaceError::NoSuchPath)
        );
        assert_eq!(
            w.list_dir(&id("f/c/z"), ""),
            Err(WorkspaceError::Missing("f/c/z".into()))
        );
        assert!(w.calls().contains(&"diff f/c/a origin/main".to_string()));
        assert!(w.calls().contains(&"read_file f/c/a src/lib.rs".to_string()));
        assert_eq!(
            WorkspaceError::TooLarge { limit: 1 << 20 }.to_string(),
            "file larger than 1 MiB"
        );
        assert_eq!(
            WorkspaceError::InvalidPath("absolute".into()).to_string(),
            "workspace: invalid path: absolute"
        );
    }
```

Run: `mise x -- cargo test -p balerix-core fakes`
Expected: compile error (no `FakeWorkspace`).

- [ ] **Step 4: The error, the port, the fake**

In `crates/balerix-core/src/ports.rs`, extend the `balerix_api` import to `use balerix_api::{CredentialBundle, GitSettings, Timestamp, WorkspaceDiff, WorkspaceTree};` and add after `RunnerError`:

```rust
/// Why a workspace read failed (Spec C §3.1). `Display` is what the
/// daemon answers a plugin with, except `Io`, which names a daemon-side
/// path and is logged instead.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceError {
    /// The agent's worktree directory does not exist.
    #[error("no workspace for agent {0}")]
    Missing(String),
    /// The worktree exists and the path does not.
    #[error("no such path")]
    NoSuchPath,
    #[error("workspace: invalid path: {0}")]
    InvalidPath(String),
    #[error("not a regular file")]
    NotAFile,
    #[error("not a directory")]
    NotADirectory,
    #[error("file larger than {} MiB", limit >> 20)]
    TooLarge { limit: u64 },
    #[error("{id}: git {subcommand}: {}", first_line(stderr))]
    Tool {
        id: String,
        subcommand: String,
        args: Vec<String>,
        stderr: String,
    },
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
}

/// Read-only access to an agent's worktree (Spec C §3.1). Sync like every
/// port; the daemon calls it in `spawn_blocking`. Implementations apply
/// `balerix_api::check_path` before any I/O and never follow a symlink.
pub trait WorkspaceReader: Send + Sync {
    /// The worktree against the merge-base with `base_ref`
    /// (`origin/<crew ref>`): committed, uncommitted and untracked alike.
    fn diff(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceDiff, WorkspaceError>;
    /// One regular file, at most `WORKSPACE_FILE_LIMIT` bytes.
    fn read_file(&self, agent: &AgentId, path: &str) -> Result<Vec<u8>, WorkspaceError>;
    /// One directory, never recursive; the empty path is the root.
    fn list_dir(&self, agent: &AgentId, path: &str) -> Result<WorkspaceTree, WorkspaceError>;
}
```

In `crates/balerix-core/src/lib.rs` add `WorkspaceError, WorkspaceReader,` to the `ports` re-export list.

In `crates/balerix-core/src/fakes.rs`: extend the `balerix_api` import to `use balerix_api::{CredentialBundle, EntryKind, GitSettings, Timestamp, TreeEntry, WORKSPACE_FILE_LIMIT, WorkspaceDiff, WorkspaceTree, check_path};` and the `crate::ports` import with `WorkspaceError, WorkspaceReader`. Before `pub struct FakeClock`:

```rust
struct FakeTree {
    diff: WorkspaceDiff,
    files: BTreeMap<String, Vec<u8>>,
}

/// An in-memory `WorkspaceReader`: per agent, the diff to answer and a
/// flat map of relative path → bytes from which `read_file` and
/// `list_dir` answer. An agent never `set` has no workspace.
#[derive(Default)]
pub struct FakeWorkspace {
    rec: Mutex<Recorder>,
    agents: Mutex<BTreeMap<String, FakeTree>>,
}

impl FakeWorkspace {
    pub fn set(&self, agent: &AgentId, diff: WorkspaceDiff, files: BTreeMap<String, Vec<u8>>) {
        lock(&self.agents).insert(agent.to_string(), FakeTree { diff, files });
    }
    pub fn calls(&self) -> Vec<String> {
        lock(&self.rec).calls.clone()
    }
    fn with<T>(
        &self,
        agent: &AgentId,
        f: impl FnOnce(&FakeTree) -> Result<T, WorkspaceError>,
    ) -> Result<T, WorkspaceError> {
        let agents = lock(&self.agents);
        match agents.get(&agent.to_string()) {
            Some(tree) => f(tree),
            None => Err(WorkspaceError::Missing(agent.to_string())),
        }
    }
}

/// The entries directly under `path` in a flat path map; `None` when
/// nothing lives there.
fn tree_of(files: &BTreeMap<String, Vec<u8>>, path: &str) -> Option<WorkspaceTree> {
    let prefix = if path.is_empty() {
        String::new()
    } else {
        format!("{path}/")
    };
    let mut entries: BTreeMap<String, TreeEntry> = BTreeMap::new();
    for (key, bytes) in files {
        let Some(rest) = key.strip_prefix(&prefix) else {
            continue;
        };
        match rest.split_once('/') {
            Some((dir, _)) => {
                entries.entry(dir.to_string()).or_insert(TreeEntry {
                    name: dir.to_string(),
                    kind: EntryKind::Dir,
                    size: None,
                });
            }
            None => {
                entries.insert(
                    rest.to_string(),
                    TreeEntry {
                        name: rest.to_string(),
                        kind: EntryKind::File,
                        size: Some(bytes.len() as u64),
                    },
                );
            }
        }
    }
    if entries.is_empty() && !path.is_empty() {
        return None;
    }
    Some(WorkspaceTree {
        path: path.to_string(),
        entries: entries.into_values().collect(),
    })
}

impl WorkspaceReader for FakeWorkspace {
    fn diff(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceDiff, WorkspaceError> {
        lock(&self.rec).record("diff", &format!("{agent} {base_ref}"));
        self.with(agent, |t| {
            Ok(WorkspaceDiff {
                base_ref: base_ref.to_string(),
                ..t.diff.clone()
            })
        })
    }
    fn read_file(&self, agent: &AgentId, path: &str) -> Result<Vec<u8>, WorkspaceError> {
        lock(&self.rec).record("read_file", &format!("{agent} {path}"));
        check_path(path).map_err(WorkspaceError::InvalidPath)?;
        self.with(agent, |t| match t.files.get(path) {
            Some(bytes) if bytes.len() as u64 > WORKSPACE_FILE_LIMIT => Err(WorkspaceError::TooLarge {
                limit: WORKSPACE_FILE_LIMIT,
            }),
            Some(bytes) => Ok(bytes.clone()),
            None if tree_of(&t.files, path).is_some() => Err(WorkspaceError::NotAFile),
            None => Err(WorkspaceError::NoSuchPath),
        })
    }
    fn list_dir(&self, agent: &AgentId, path: &str) -> Result<WorkspaceTree, WorkspaceError> {
        lock(&self.rec).record("list_dir", &format!("{agent} {path}"));
        check_path(path).map_err(WorkspaceError::InvalidPath)?;
        self.with(agent, |t| {
            if t.files.contains_key(path) {
                return Err(WorkspaceError::NotADirectory);
            }
            tree_of(&t.files, path).ok_or(WorkspaceError::NoSuchPath)
        })
    }
}
```

`Recorder::record` returns the armed failure; `FakeWorkspace` ignores it (no `fail_next` is needed by any test in this plan), so the calls above discard the return value — write them as `let _ = lock(&self.rec).record(...)` if clippy asks for it.

The path rule's property (Spec C §7): in `crates/balerix-core/src/ports.rs`'s `mod tests` add (core already has `proptest` as a dev-dependency):

```rust
    mod path_rule {
        use balerix_api::check_path;
        use proptest::prelude::*;
        use std::path::{Component, Path};

        fn segment() -> impl Strategy<Value = String> {
            prop_oneof![
                4 => "[a-z][a-z0-9._-]{0,6}".prop_map(String::from),
                1 => Just("..".to_string()),
                1 => Just(".".to_string()),
                1 => Just(String::new()),
                1 => Just(".git".to_string()),
                1 => Just(".GIT".to_string()),
            ]
        }

        proptest! {
            /// An accepted path joins under the root without `.`/`..`
            /// components, names no `.git` and has no empty segment; a
            /// refused one carries a reason.
            #[test]
            fn accepted_paths_stay_under_the_root(segments in prop::collection::vec(segment(), 0..6)) {
                let path = segments.join("/");
                match check_path(&path) {
                    Ok(()) => {
                        let joined = Path::new("/root").join(&path);
                        prop_assert!(joined
                            .components()
                            .all(|c| !matches!(c, Component::ParentDir | Component::CurDir)));
                        prop_assert!(joined.starts_with("/root"));
                        prop_assert!(!path.split('/').any(|s| s.eq_ignore_ascii_case(".git")));
                        prop_assert!(path.is_empty() || !path.split('/').any(str::is_empty));
                    }
                    Err(reason) => prop_assert!(!reason.is_empty()),
                }
            }
        }
    }
```

Run: `mise x -- cargo test -p balerix-core` and `mise x -- cargo clippy -p balerix-api -p balerix-core --all-targets -- -D warnings`
Expected: PASS, clean.

- [ ] **Step 5: Commit**

```bash
git add crates/balerix-api crates/balerix-core
git commit -q -F - > "$SCRATCH/commit.log" 2>&1 <<'EOF'
Add the workspace wire types, the path rule and the WorkspaceReader port

Spec C §2.2 and §3.1: `WorkspaceDiff`/`WorkspaceTree` in balerix-api with
the `check_path` rule beside them (the web plugin needs the rule too and
may depend on balerix-api only), `Capability::Workspace`, and in
balerix-core the `WorkspaceReader` port, `WorkspaceError` and an in-memory
`FakeWorkspace` for the server tests.

Claude-Session: https://claude.ai/code/session_01XVWfZKBMR5cciaB4H5cXz5
EOF
grep -E "Summary|FAIL" "$SCRATCH/commit.log"; git log --oneline -1
```

---

### Task 2: `WorkspaceReader` over real git

**Files:**
- Create: `crates/balerix-runtime/src/inspect.rs`
- Modify: `crates/balerix-runtime/src/tools.rs` (`Cmd::run`), `crates/balerix-runtime/src/lib.rs`
- Create: `crates/balerix-runtime/tests/inspect_it.rs`

**Interfaces:**
- Consumes: `Runtime { layout, tools }`, `StateLayout::{agent, crew}`, `Cmd`, `balerix_api::{check_path, limits, types}`, `balerix_core::{WorkspaceError, WorkspaceReader}`.
- Produces:
  ```rust
  // balerix_runtime::tools (crate-private)
  impl Cmd { pub(crate) fn run_with_exit_codes(&self, accepted: &[i32]) -> Result<CmdOutput, CmdFailure>; }  // run() == run_with_exit_codes(&[0])
  // balerix_runtime::inspect
  impl WorkspaceReader for Runtime { … }
  pub fn parse_name_status(z: &str) -> Vec<FileDiff>;               // `diff --name-status -z --find-renames`
  pub fn shape_patch(raw: String) -> (String, bool, bool);          // (patch, binary, truncated)
  ```

- [ ] **Step 1: Write the failing unit tests**

Create `crates/balerix-runtime/src/inspect.rs` with the module doc and the tests:

```rust
//! `WorkspaceReader` over real git (Spec C §3.2): the agent's worktree
//! against the crew's base, one file, one listing. Reads only, with the
//! repository's config escape hatches closed — an agent can write the
//! shared `.git/config` and `.gitattributes`, and this code runs as the
//! daemon.

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::FileStatus;

    #[test]
    fn name_status_z_parses_statuses_and_rename_pairs() {
        let z = "M\0README\0R100\0LICENSE\0COPYING\0A\0src/lib.rs\0D\0gone\0T\0link\0C75\0a\0b\0";
        let files = parse_name_status(z);
        let got: Vec<(&str, Option<&str>, FileStatus)> = files
            .iter()
            .map(|f| (f.path.as_str(), f.old_path.as_deref(), f.status))
            .collect();
        assert_eq!(
            got,
            vec![
                ("README", None, FileStatus::Modified),
                ("COPYING", Some("LICENSE"), FileStatus::Renamed),
                ("src/lib.rs", None, FileStatus::Added),
                ("gone", None, FileStatus::Deleted),
                ("link", None, FileStatus::Typechange),
                ("b", Some("a"), FileStatus::Copied),
            ]
        );
        assert!(files.iter().all(|f| !f.uncommitted && !f.binary && f.patch.is_empty()));
        assert!(parse_name_status("").is_empty());
        assert_eq!(parse_name_status("R100\0only-old\0").len(), 0, "a torn pair is dropped");
    }

    #[test]
    fn a_patch_is_marked_binary_or_cut_at_a_line_boundary() {
        assert_eq!(
            shape_patch("diff --git a/x b/x\nBinary files a/x and b/x differ\n".into()),
            (String::new(), true, false)
        );
        let small = "diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n".to_string();
        assert_eq!(shape_patch(small.clone()), (small, false, false));
        let line = format!("+{}\n", "y".repeat(99));
        let big = line.repeat(WORKSPACE_PATCH_LIMIT / 100 + 10);
        let (cut, binary, truncated) = shape_patch(big);
        assert!(!binary && truncated);
        assert!(cut.len() <= WORKSPACE_PATCH_LIMIT);
        assert!(cut.ends_with('\n'), "cut at a line boundary");
        assert_eq!(cut.len() % 100, 0);
    }
}
```

Add `pub mod inspect;` to `crates/balerix-runtime/src/lib.rs` (alphabetically, after `home`).

Run: `mise x -- cargo test -p balerix-runtime --lib inspect`
Expected: compile error.

- [ ] **Step 2: `Cmd::run_with_exit_codes`**

In `crates/balerix-runtime/src/tools.rs`, rename the body of `run` and add the wrapper:

```rust
    pub(crate) fn run(&self) -> Result<CmdOutput, CmdFailure> {
        self.run_with_exit_codes(&[0])
    }

    /// `run`, treating any exit code in `accepted` as success: `git diff
    /// --no-index` exits 1 when the files differ, which is the answer.
    pub(crate) fn run_with_exit_codes(&self, accepted: &[i32]) -> Result<CmdOutput, CmdFailure> {
        // … the former body of `run`, unchanged up to the status check …
        let ok = out.status.code().is_some_and(|c| accepted.contains(&c));
        if !ok {
            return Err(failure(if stderr.trim().is_empty() {
                format!("exit status {}", out.status)
            } else {
                stderr
            }));
        }
        Ok(CmdOutput { stdout })
    }
```

(replace `if !out.status.success()` with the `ok` check; nothing else moves). In the existing `tools.rs` tests add:

```rust
    #[test]
    fn an_accepted_exit_code_is_success_and_keeps_stdout() {
        let sh = Cmd::new(Path::new("/bin/sh")).args(["-c", "echo out; exit 1"]);
        assert!(sh.run().is_err());
        assert_eq!(sh.run_with_exit_codes(&[0, 1]).unwrap().stdout, "out\n");
        assert!(sh.run_with_exit_codes(&[2]).is_err());
    }
```

Run: `mise x -- cargo test -p balerix-runtime --lib tools`
Expected: PASS.

- [ ] **Step 3: The implementation**

Above the test module in `inspect.rs`:

```rust
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use balerix_api::{
    EntryKind, FileDiff, FileStatus, TreeEntry, WORKSPACE_FILE_COUNT_LIMIT, WORKSPACE_FILE_LIMIT,
    WORKSPACE_PATCH_LIMIT, WorkspaceDiff, WorkspaceTree, check_path,
};
use balerix_core::{AgentId, WorkspaceError, WorkspaceReader};

use crate::layout::CrewPaths;
use crate::materializer::Runtime;
use crate::tools::Cmd;

/// Command-line config beats every config file: whatever an agent wrote
/// into the shared `.git/config`, no program runs from it here.
const CONFIG: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.quotePath=true",
    "-c",
    "diff.noprefix=false",
];
/// On every `diff`: no external diff driver, no textconv, no colour.
const DIFF_FLAGS: &[&str] = &["--no-ext-diff", "--no-textconv", "--no-color"];

impl Runtime {
    /// The agent's worktree and crew paths; `Missing` when the worktree
    /// directory does not exist (not materialized, or purged).
    fn workspace_of(&self, agent: &AgentId) -> Result<(PathBuf, CrewPaths), WorkspaceError> {
        let paths = self.layout.agent(agent);
        if !paths.workspace.is_dir() {
            return Err(WorkspaceError::Missing(agent.to_string()));
        }
        Ok((paths.workspace, self.layout.crew(&agent.crew_ref())))
    }

    /// One git call in the worktree, logged to the crew's `git.log` like
    /// `Workspace::git`, with the same `GIT_*` scrub, no optional locks,
    /// fsmonitor off and hooks pointed at an empty directory.
    fn inspect_git(
        &self,
        id: &str,
        crew: &CrewPaths,
        workspace: &Path,
        args: &[&str],
        accepted: &[i32],
    ) -> Result<String, WorkspaceError> {
        let no_hooks = crew.root.join("no-hooks");
        let _ = std::fs::create_dir_all(&no_hooks);
        let mut cmd = Cmd::new(&self.tools.git).log(&crew.root.join("logs").join("git.log"));
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_PREFIX",
            "GIT_COMMON_DIR",
        ] {
            cmd = cmd.env_remove(var);
        }
        cmd = cmd
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(CONFIG.iter().copied())
            .args([
                "-c".to_string(),
                format!("core.hooksPath={}", no_hooks.display()),
            ])
            .args(["-C".to_string(), workspace.display().to_string()])
            .args(args.iter().copied());
        cmd.run_with_exit_codes(accepted)
            .map(|o| o.stdout)
            .map_err(|f| WorkspaceError::Tool {
                id: id.to_string(),
                subcommand: f.subcommand,
                args: f.args,
                stderr: f.stderr,
            })
    }
}

fn split_z(s: &str) -> BTreeSet<String> {
    s.split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// `diff --name-status -z --find-renames` → one `FileDiff` per record,
/// patches empty. `R`/`C` records carry two paths; a torn pair ends the
/// parse.
pub fn parse_name_status(z: &str) -> Vec<FileDiff> {
    let mut out = Vec::new();
    let mut parts = z.split('\0').filter(|p| !p.is_empty());
    while let Some(code) = parts.next() {
        let status = match code.chars().next() {
            Some('A') => FileStatus::Added,
            Some('D') => FileStatus::Deleted,
            Some('R') => FileStatus::Renamed,
            Some('C') => FileStatus::Copied,
            Some('T') => FileStatus::Typechange,
            _ => FileStatus::Modified,
        };
        let (old_path, path) = match status {
            FileStatus::Renamed | FileStatus::Copied => match (parts.next(), parts.next()) {
                (Some(old), Some(new)) => (Some(old.to_string()), new.to_string()),
                _ => break,
            },
            _ => match parts.next() {
                Some(p) => (None, p.to_string()),
                None => break,
            },
        };
        out.push(FileDiff {
            path,
            old_path,
            status,
            uncommitted: false,
            binary: false,
            patch: String::new(),
            truncated: false,
        });
    }
    out
}

/// `(patch, binary, truncated)`: a binary diff keeps no patch; a long one
/// is cut at the last line boundary under `WORKSPACE_PATCH_LIMIT`.
pub fn shape_patch(raw: String) -> (String, bool, bool) {
    if raw
        .lines()
        .any(|l| l.starts_with("Binary files ") || l.starts_with("GIT binary patch"))
    {
        return (String::new(), true, false);
    }
    if raw.len() <= WORKSPACE_PATCH_LIMIT {
        return (raw, false, false);
    }
    let end = raw[..WORKSPACE_PATCH_LIMIT]
        .rfind('\n')
        .map_or(0, |i| i + 1);
    (raw[..end].to_string(), false, true)
}

fn io_error(path: &Path, e: std::io::Error) -> WorkspaceError {
    WorkspaceError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// `symlink_metadata`: never follows the final component.
fn meta_of(path: &Path) -> Result<std::fs::Metadata, WorkspaceError> {
    std::fs::symlink_metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            WorkspaceError::NoSuchPath
        } else {
            io_error(path, e)
        }
    })
}

/// Defence in depth against a symlinked ancestor: the canonical path must
/// still sit under the canonical worktree.
fn confine(workspace: &Path, full: &Path) -> Result<(), WorkspaceError> {
    let root = workspace
        .canonicalize()
        .map_err(|e| io_error(workspace, e))?;
    let real = full.canonicalize().map_err(|e| io_error(full, e))?;
    if real.starts_with(&root) {
        Ok(())
    } else {
        Err(WorkspaceError::InvalidPath("escapes the worktree".into()))
    }
}

impl WorkspaceReader for Runtime {
    fn diff(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceDiff, WorkspaceError> {
        let id = agent.to_string();
        let (ws, crew) = self.workspace_of(agent)?;
        let git = |args: &[&str], ok: &[i32]| self.inspect_git(&id, &crew, &ws, args, ok);
        let head = git(&["rev-parse", "HEAD"], &[0])?.trim().to_string();
        let merge_base = git(&["merge-base", base_ref, "HEAD"], &[0])?
            .trim()
            .to_string();
        let mut files = parse_name_status(&git(
            &["diff", "--name-status", "-z", "--find-renames", &merge_base],
            &[0],
        )?);
        let dirty = split_z(&git(&["diff", "--name-only", "-z", "HEAD"], &[0])?);
        let untracked = split_z(&git(
            &["ls-files", "--others", "--exclude-standard", "-z"],
            &[0],
        )?);
        for path in &untracked {
            files.push(FileDiff {
                path: path.clone(),
                old_path: None,
                status: FileStatus::Added,
                uncommitted: true,
                binary: false,
                patch: String::new(),
                truncated: false,
            });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let truncated = files.len() > WORKSPACE_FILE_COUNT_LIMIT;
        files.truncate(WORKSPACE_FILE_COUNT_LIMIT);
        for f in &mut files {
            f.uncommitted = f.uncommitted
                || dirty.contains(&f.path)
                || f.old_path.as_deref().is_some_and(|o| dirty.contains(o));
            let mut args: Vec<&str> = vec!["diff"];
            args.extend(DIFF_FLAGS);
            let raw = if untracked.contains(&f.path) {
                args.extend(["--no-index", "-U3", "--", "/dev/null", f.path.as_str()]);
                git(&args, &[0, 1])?
            } else {
                args.extend(["-U3", "--find-renames", merge_base.as_str(), "--"]);
                if let Some(old) = &f.old_path {
                    args.push(old);
                }
                args.push(&f.path);
                git(&args, &[0])?
            };
            let (patch, binary, cut) = shape_patch(raw);
            f.patch = patch;
            f.binary = binary;
            f.truncated = cut;
        }
        Ok(WorkspaceDiff {
            base_ref: base_ref.to_string(),
            merge_base,
            head,
            files,
            truncated,
        })
    }

    fn read_file(&self, agent: &AgentId, path: &str) -> Result<Vec<u8>, WorkspaceError> {
        check_path(path).map_err(WorkspaceError::InvalidPath)?;
        let (ws, _) = self.workspace_of(agent)?;
        let full = ws.join(path);
        let meta = meta_of(&full)?;
        if !meta.is_file() {
            return Err(WorkspaceError::NotAFile);
        }
        confine(&ws, &full)?;
        if meta.len() > WORKSPACE_FILE_LIMIT {
            return Err(WorkspaceError::TooLarge {
                limit: WORKSPACE_FILE_LIMIT,
            });
        }
        std::fs::read(&full).map_err(|e| io_error(&full, e))
    }

    fn list_dir(&self, agent: &AgentId, path: &str) -> Result<WorkspaceTree, WorkspaceError> {
        check_path(path).map_err(WorkspaceError::InvalidPath)?;
        let (ws, _) = self.workspace_of(agent)?;
        let full = if path.is_empty() {
            ws.clone()
        } else {
            ws.join(path)
        };
        if !meta_of(&full)?.is_dir() {
            return Err(WorkspaceError::NotADirectory);
        }
        confine(&ws, &full)?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&full).map_err(|e| io_error(&full, e))? {
            let entry = entry.map_err(|e| io_error(&full, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            // `DirEntry::metadata` does not follow symlinks.
            let m = entry.metadata().map_err(|e| io_error(&entry.path(), e))?;
            let kind = if m.is_file() {
                EntryKind::File
            } else if m.is_dir() {
                EntryKind::Dir
            } else if m.file_type().is_symlink() {
                EntryKind::Symlink
            } else {
                EntryKind::Other
            };
            entries.push(TreeEntry {
                name,
                kind,
                size: m.is_file().then(|| m.len()),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(WorkspaceTree {
            path: path.to_string(),
            entries,
        })
    }
}
```

Run: `mise x -- cargo test -p balerix-runtime --lib inspect` and clippy for the crate.
Expected: PASS, clean.

- [ ] **Step 4: Write the failing integration test**

Create `crates/balerix-runtime/tests/inspect_it.rs`:

```rust
//! Spec C §3.2 against real git: the five change kinds, the path refusals
//! and the fsmonitor control.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use balerix_api::{EntryKind, FileStatus, WORKSPACE_FILE_LIMIT};
use balerix_core::{AgentId, RepoRef, WorkspaceError, WorkspaceReader};
use balerix_runtime::{Runtime, Workspace};

fn git(dir: &Path, args: &[&str]) -> String {
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
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A bare repo with README and LICENSE on `main`, served over file://.
fn bare_repo(root: &Path) -> RepoRef {
    let work = root.join("upstream-work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README"), "hi\n").unwrap();
    std::fs::write(work.join("LICENSE"), "mit\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("upstream.git");
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    RepoRef::parse(&format!("file://{}", bare.display())).unwrap()
}

#[test]
fn the_diff_reports_every_change_kind_and_reads_stay_inside_the_worktree() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("inspect");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    let rt = Runtime::new(layout.clone(), tools.clone());

    // no worktree yet
    assert_eq!(
        rt.diff(&id, "origin/main"),
        Err(WorkspaceError::Missing("f/c/a".into()))
    );

    ws.ensure_repo("f/c/a", &crew, &repo, "main").unwrap();
    ws.ensure_worktree("f/c/a", &crew, &paths.workspace, "balerix/f/c/a", "main")
        .unwrap();
    let w = &paths.workspace;
    let base = git(w, &["rev-parse", "origin/main"]).trim().to_string();

    // committed: a rename, a modification, a binary, a new file
    git(w, &["mv", "LICENSE", "COPYING"]);
    std::fs::write(w.join("README"), "hi\nmore\n").unwrap();
    std::fs::write(w.join("img.bin"), [0u8, 1, 2, 255, 0, 7]).unwrap();
    std::fs::create_dir_all(w.join("src")).unwrap();
    std::fs::write(w.join("src/lib.rs"), "fn a() {}\n").unwrap();
    git(w, &["add", "-A"]);
    git(w, &["commit", "-q", "-m", "agent work"]);
    let head = git(w, &["rev-parse", "HEAD"]).trim().to_string();
    // uncommitted: an edit and an untracked file
    std::fs::write(w.join("src/lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    std::fs::write(w.join("notes.txt"), "todo\n").unwrap();

    let d = rt.diff(&id, "origin/main").unwrap();
    assert_eq!((d.base_ref.as_str(), d.merge_base, d.head), ("origin/main", base, head));
    assert!(!d.truncated);
    let names: Vec<&str> = d.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        names,
        vec!["COPYING", "README", "img.bin", "notes.txt", "src/lib.rs"],
        "path order"
    );
    let by = |p: &str| d.files.iter().find(|f| f.path == p).unwrap();
    let copying = by("COPYING");
    assert_eq!(
        (copying.status, copying.old_path.as_deref(), copying.uncommitted),
        (FileStatus::Renamed, Some("LICENSE"), false)
    );
    assert!(copying.patch.contains("rename from LICENSE"), "{}", copying.patch);
    let readme = by("README");
    assert_eq!((readme.status, readme.uncommitted, readme.binary), (FileStatus::Modified, false, false));
    assert!(readme.patch.contains("+more"), "{}", readme.patch);
    let img = by("img.bin");
    assert_eq!((img.status, img.binary, img.patch.as_str()), (FileStatus::Added, true, ""));
    let notes = by("notes.txt");
    assert_eq!((notes.status, notes.uncommitted), (FileStatus::Added, true));
    assert!(notes.patch.contains("+todo"), "{}", notes.patch);
    let lib = by("src/lib.rs");
    assert_eq!((lib.status, lib.uncommitted), (FileStatus::Added, true));
    assert!(lib.patch.contains("+fn b() {}"), "{}", lib.patch);
    assert!(
        d.files.iter().all(|f| f.patch.is_empty() || f.patch.starts_with("diff --git ")),
        "every patch carries its header"
    );
    // what `git diff` itself says, minus nothing
    assert_eq!(
        readme.patch,
        git(w, &["diff", "--no-color", "-U3", &d.merge_base, "--", "README"])
    );

    // file and tree
    assert_eq!(rt.read_file(&id, "src/lib.rs").unwrap(), b"fn a() {}\nfn b() {}\n".to_vec());
    assert_eq!(rt.read_file(&id, "nope"), Err(WorkspaceError::NoSuchPath));
    assert_eq!(rt.read_file(&id, "src"), Err(WorkspaceError::NotAFile));
    assert_eq!(
        rt.read_file(&id, ".git"),
        Err(WorkspaceError::InvalidPath(".git segment".into())),
        "the worktree's .git file is refused before any I/O"
    );
    assert_eq!(
        rt.read_file(&id, "../../repo/HEAD"),
        Err(WorkspaceError::InvalidPath("\"..\" segment".into()))
    );
    std::os::unix::fs::symlink("/etc/hostname", w.join("escape")).unwrap();
    assert_eq!(rt.read_file(&id, "escape"), Err(WorkspaceError::NotAFile), "symlinks are not followed");
    std::os::unix::fs::symlink(&crew.repo, w.join("repo-link")).unwrap();
    assert_eq!(rt.list_dir(&id, "repo-link"), Err(WorkspaceError::NotADirectory));
    std::fs::write(w.join("big"), vec![b'x'; (WORKSPACE_FILE_LIMIT + 1) as usize]).unwrap();
    assert_eq!(
        rt.read_file(&id, "big"),
        Err(WorkspaceError::TooLarge { limit: WORKSPACE_FILE_LIMIT })
    );
    let tree = rt.list_dir(&id, "").unwrap();
    let names: Vec<(&str, EntryKind)> = tree.entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
    assert!(!names.iter().any(|(n, _)| *n == ".git"), "{names:?}");
    assert!(names.contains(&("src", EntryKind::Dir)));
    assert!(names.contains(&("escape", EntryKind::Symlink)));
    assert!(names.contains(&("img.bin", EntryKind::File)));
    let src = rt.list_dir(&id, "src").unwrap();
    assert_eq!(src.path, "src");
    assert_eq!(src.entries[0].name, "lib.rs");
    assert_eq!(src.entries[0].size, Some(20));
    assert_eq!(rt.list_dir(&id, "README"), Err(WorkspaceError::NotADirectory));
    assert_eq!(rt.list_dir(&id, "nope"), Err(WorkspaceError::NoSuchPath));

    // repo config an agent could write must not run a program here
    let marker = root.join("fsmonitor-ran");
    let hook = root.join("fsmonitor.sh");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\ntouch {}\necho\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(w, &["config", "core.fsmonitor", &hook.display().to_string()]);
    git(w, &["config", "diff.external", &hook.display().to_string()]);
    let again = rt.diff(&id, "origin/main").unwrap();
    assert_eq!(again.files.len(), d.files.len() + 1, "big joined");
    assert!(!marker.exists(), "core.fsmonitor / diff.external from repo config ran");
    assert!(again.files.iter().find(|f| f.path == "README").unwrap().patch.contains("+more"));

    // a missing base is a git error naming the subcommand
    let e = rt.diff(&id, "origin/nope").unwrap_err();
    assert!(
        matches!(&e, WorkspaceError::Tool { subcommand, .. } if subcommand == "merge-base"),
        "{e}"
    );
}
```

Run: `mise x -- cargo test -p balerix-runtime --test inspect_it`
Expected: PASS against the real git on `PATH` (`BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --test inspect_it` in CI). If `src.entries[0].size` differs, count the bytes of `"fn a() {}\nfn b() {}\n"` (20) — do not weaken the assertion.

- [ ] **Step 5: Commit**

```bash
git add crates/balerix-runtime
git commit -q -F - > "$SCRATCH/commit.log" 2>&1 <<'EOF'
Implement WorkspaceReader over git in the runtime

Spec C §3.2: `Runtime` answers `diff` as the worktree against the
merge-base with `origin/<ref>` (a name-status pass, the dirty and
untracked sets, one bounded per-file patch each), `read_file` and
`list_dir` under the path rule with symlinks never followed and a
canonical-prefix check. Every git call scrubs the `GIT_*` variables,
sets `GIT_OPTIONAL_LOCKS=0`, turns fsmonitor off and points hooks at an
empty directory, and every diff runs with `--no-ext-diff --no-textconv`,
because the repository's config is something an agent can write.
`Cmd::run_with_exit_codes` lets `diff --no-index` exit 1 as an answer.

Claude-Session: https://claude.ai/code/session_01XVWfZKBMR5cciaB4H5cXz5
EOF
grep -E "Summary|FAIL" "$SCRATCH/commit.log"; git log --oneline -1
```

---

### Task 3: The daemon's workspace routes

**Files:**
- Modify: `crates/balerix-server/src/actor.rs:64-73` (`Ports`)
- Modify: `crates/balerix-server/src/plugins/host.rs:86-94` (the `Ports` clone)
- Modify: `crates/balerix-server/src/testing.rs` (`Harness` fields, both `Ports` literals)
- Modify: `crates/balerix-server/tests/plugins_it.rs:360-368` (the `Ports` literal)
- Modify: `crates/balerix/src/commands/serve.rs:161-169`
- Modify: `crates/balerix-server/src/daemon.rs` (after `runner()`)
- Modify: `crates/balerix-server/src/api.rs` (after `From<PluginError>`)
- Modify: `crates/balerix-server/src/plugin_api.rs`
- Modify: `crates/balerix-server/tests/support/mod.rs` (`world()`'s web package)
- Create: `crates/balerix-server/tests/workspace_it.rs`

**Interfaces:**
- Consumes: `balerix_core::{WorkspaceError, WorkspaceReader, fakes::FakeWorkspace}`, `Capability::Workspace`, `PluginRegistry::{has, is_active}`, `caller()` in `plugin_api.rs`.
- Produces:
  ```rust
  // balerix_server::actor
  pub struct Ports { …, pub workspace: Arc<dyn WorkspaceReader> }
  // balerix_server::daemon
  impl Daemon {
      pub fn workspace(&self) -> Arc<dyn WorkspaceReader>;
      /// `origin/<crew ref>` for the agent's crew; None for an unknown fleet or crew.
      pub async fn base_ref(&self, agent: &AgentId) -> Option<String>;
  }
  // balerix_server::testing
  pub struct Harness { …, pub workspace: Arc<FakeWorkspace> }
  // routes (plugin_api.rs), plugin bearer, `workspace` capability, active pair:
  //   GET /v1/plugin-host/agents/{f}/{c}/{a}/workspace/diff           → WorkspaceDiff
  //   GET /v1/plugin-host/agents/{f}/{c}/{a}/workspace/file?path=     → bytes, application/octet-stream
  //   GET /v1/plugin-host/agents/{f}/{c}/{a}/workspace/tree?path=     → WorkspaceTree
  // status mapping (api.rs): Missing/NoSuchPath 404, InvalidPath/NotAFile/NotADirectory 400,
  //   TooLarge 413, Tool 500 (message), Io 500 "workspace: storage error" (path logged)
  ```

- [ ] **Step 1: `Ports.workspace` everywhere**

In `crates/balerix-server/src/actor.rs` add to `Ports` (after `store`):

```rust
    /// Read-only worktree access for the plugin host's workspace routes
    /// (Spec C §3.1). Not used by the reconciler.
    pub workspace: Arc<dyn WorkspaceReader>,
```

with `WorkspaceReader` added to the `balerix_core` import. In `crates/balerix-server/src/plugins/host.rs` the `Ports` literal gains `workspace: agent_ports.workspace.clone(),`. In `crates/balerix-server/src/testing.rs`: `Harness` gains `pub workspace: Arc<FakeWorkspace>,`; `with_policy` builds `let workspace = Arc::new(FakeWorkspace::default());`, both `Ports` literals gain `workspace: workspace.clone()` / `workspace: self.workspace.clone()`, and the struct literal stores it (import `FakeWorkspace` from `balerix_core::fakes`). In `crates/balerix-server/tests/plugins_it.rs` the literal gains `workspace: h.workspace.clone(),`. In `crates/balerix/src/commands/serve.rs`:

```rust
        let runtime = Arc::new(Runtime::new(layout.clone(), tools.clone()));
        let ports = Ports {
            materializer: runtime.clone(),
            runner: Arc::new(TmuxRunner::new(tools.tmux.clone(), tmux_socket)),
            clock: Arc::new(SystemClock),
            store: Arc::new(store),
            workspace: runtime,
            policy: ReconcilePolicy::default(),
            hook_url: url.clone(),
            resync: RESYNC,
        };
```

In `crates/balerix-server/src/daemon.rs`, after `pub fn runner`:

```rust
    pub fn workspace(&self) -> Arc<dyn WorkspaceReader> {
        self.ports.workspace.clone()
    }

    /// `origin/<ref>` of the agent's crew, from the fleet's record; `None`
    /// when the fleet or the crew is unknown.
    pub async fn base_ref(&self, agent: &AgentId) -> Option<String> {
        let record = self.get(&agent.fleet).await?;
        let crew = record.spec.crews.get(agent.crew.as_str())?;
        Some(format!("origin/{}", crew.git_ref))
    }
```

Run: `mise x -- cargo build --workspace --all-targets`
Expected: builds (every `Ports` literal updated).

- [ ] **Step 2: Write the failing server test**

In `crates/balerix-server/tests/support/mod.rs`, `world()`, change the web package's manifest extra to:

```rust
        "hooks: { observe: [SessionStart] }\nneeds: [fleets, attach, workspace]\nroutes: true\n",
```

and update the doc comment above `world()` ("needs fleets+attach+workspace"). Create `crates/balerix-server/tests/workspace_it.rs`:

```rust
//! Spec C §2.2 against the real daemon: the capability gate, the active
//! pair, the two 404s, the 413 and the 400, with `FakeWorkspace` behind.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use balerix_api::{
    ActivationState, AgentSettings, CrewSpec, FileDiff, FileStatus, FleetRequest, FleetSpec,
    GitSettings, WORKSPACE_FILE_LIMIT, WorkspaceDiff,
};
use balerix_core::plugin_id;
use balerix_plugin_sdk::{Env, Host, Plugin, bind, run};
use serde_json::{Value, json};
use support::{World, world};

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

fn spec(agents: &[&str]) -> FleetSpec {
    FleetSpec {
        name: "f".into(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/x".into(),
                git_ref: "release".into(),
                git: GitSettings::default(),
                agents: agents
                    .iter()
                    .map(|n| {
                        (
                            n.to_string(),
                            AgentSettings {
                                plugins: BTreeMap::from([("web".to_string(), json!({}))]),
                                ..Default::default()
                            },
                        )
                    })
                    .collect(),
            },
        )]),
    }
}

fn diff() -> WorkspaceDiff {
    WorkspaceDiff {
        base_ref: String::new(),
        merge_base: "m".repeat(40),
        head: "h".repeat(40),
        files: vec![FileDiff {
            path: "src/lib.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            uncommitted: true,
            binary: false,
            patch: "diff --git a/src/lib.rs b/src/lib.rs\n@@ -1 +1 @@\n-a\n+b\n".into(),
            truncated: false,
        }],
        truncated: false,
    }
}

async fn wait_active(w: &World, agent: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let rec = w.daemon.get(&"f".parse().unwrap()).await;
            if rec.as_ref().is_some_and(|r| {
                r.status.agents.get(agent).is_some_and(|a| {
                    a.plugins
                        .get("web")
                        .is_some_and(|p| p.state == ActivationState::Active)
                })
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("web active for the agent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_routes_are_gated_by_capability_activation_and_the_path_rule() {
    let w = world().await;
    let web = start_silent(&w, "web").await;
    let flow = start_silent(&w, "flow").await;
    let (s, v) = w.api.admin(
        "POST",
        "/v1/fleets",
        Some(&json!(FleetRequest {
            spec: spec(&["a", "b"]),
            credentials: Default::default()
        })),
    );
    assert_eq!(s, 200, "{v}");
    wait_active(&w, "f/c/a").await;
    wait_active(&w, "f/c/b").await;
    w.h.workspace.set(
        &"f/c/a".parse().unwrap(),
        diff(),
        BTreeMap::from([
            ("src/lib.rs".to_string(), b"fn a() {}\n".to_vec()),
            ("big".to_string(), vec![0u8; (WORKSPACE_FILE_LIMIT + 1) as usize]),
        ]),
    );

    // the diff, with the crew's ref as the base
    let got = web.workspace_diff("f/c/a").await.unwrap();
    assert_eq!(got.base_ref, "origin/release");
    assert_eq!(got.files, diff().files);
    assert!(
        w.h.workspace
            .calls()
            .contains(&"diff f/c/a origin/release".to_string())
    );
    // file and tree
    assert_eq!(
        web.workspace_file("f/c/a", "src/lib.rs").await.unwrap(),
        Some(b"fn a() {}\n".to_vec())
    );
    assert_eq!(web.workspace_file("f/c/a", "nope").await.unwrap(), None);
    let tree = web.workspace_tree("f/c/a", "").await.unwrap();
    assert_eq!(tree.entries.len(), 2);
    assert_eq!(tree.entries[1].name, "src");
    // the raw statuses: 403 without the capability, 404 for an active pair
    // with no worktree, 404 for a pair that is not active, 413, 400
    let web_tok = token(&w, "web").await;
    let flow_tok = token(&w, "flow").await;
    let (s, v) = w.api.plugin(
        &flow_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/a/workspace/diff",
        None,
    );
    assert_eq!(s, 403, "{v}");
    assert_eq!(
        v["error"],
        "capability \"workspace\" not declared in balerix-plugin.yaml"
    );
    let e = flow.workspace_diff("f/c/a").await.unwrap_err();
    assert!(e.to_string().starts_with("daemon: HTTP 403"), "{e}");
    let (s, v) = w.api.plugin(
        &web_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/b/workspace/diff",
        None,
    );
    assert_eq!((s, v["error"].as_str()), (404, Some("no workspace for agent f/c/b")));
    let (s, v) = w.api.plugin(
        &web_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/zed/workspace/diff",
        None,
    );
    assert_eq!((s, v["error"].as_str()), (404, Some("plugin is not active for agent f/c/zed")));
    let (s, v) = w.api.plugin(
        &web_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/a/workspace/file?path=big",
        None,
    );
    assert_eq!((s, v["error"].as_str()), (413, Some("file larger than 1 MiB")));
    let (s, v) = w.api.plugin(
        &web_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/a/workspace/file?path=../x",
        None,
    );
    assert_eq!(
        (s, v["error"].as_str()),
        (400, Some("workspace: invalid path: \"..\" segment"))
    );
    let (s, v) = w.api.plugin(
        &web_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/a/workspace/tree?path=src/lib.rs",
        None,
    );
    assert_eq!((s, v["error"].as_str()), (400, Some("not a directory")));
    let (s, v) = w.api.plugin(
        &web_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/a/workspace/file?path=nope",
        None,
    );
    assert_eq!((s, v["error"].as_str()), (404, Some("no such path")));
    let (s, _) = w.api.plugin(
        "nope",
        "GET",
        "/v1/plugin-host/agents/f/c/a/workspace/diff",
        None,
    );
    assert_eq!(s, 401);
    // the bytes route is raw, not JSON
    let (s, bytes) = w.api.raw_get(
        &web_tok,
        "/v1/plugin-host/agents/f/c/a/workspace/file?path=src/lib.rs",
    );
    assert_eq!((s, bytes.as_slice()), (200, &b"fn a() {}\n"[..]));
}
```

(`use serde_json::json;` — `Value` is not needed.)

(`Host::workspace_*` arrive in Task 4; until then this test does not compile. Write the routes first, then Task 4's SDK step, then run this test — or, to keep this task green on its own, temporarily assert only the raw `w.api.plugin` calls and add the `Host` calls back in Task 4. Prefer the second: land the raw assertions here, add the four `web.workspace_*`/`flow.workspace_diff` blocks in Task 4 step 5.)

Run: `mise x -- cargo test -p balerix-server --test workspace_it`
Expected: FAIL — 404 from axum for the unknown route.

- [ ] **Step 3: The routes and the error mapping**

In `crates/balerix-server/src/api.rs`, after `impl From<PluginError> for ApiError`:

```rust
impl From<balerix_core::WorkspaceError> for ApiError {
    fn from(e: balerix_core::WorkspaceError) -> Self {
        use balerix_core::WorkspaceError as W;
        // An `Io` names a daemon-side path; the plugin gets a fixed line
        // and the operator the real one in the log, as for kv.
        if let W::Io { path, message } = &e {
            tracing::warn!(path = %path.display(), "workspace read error: {message}");
            return Self::new(StatusCode::INTERNAL_SERVER_ERROR, "workspace: storage error");
        }
        let status = match &e {
            W::Missing(_) | W::NoSuchPath => StatusCode::NOT_FOUND,
            W::InvalidPath(_) | W::NotAFile | W::NotADirectory => StatusCode::BAD_REQUEST,
            W::TooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            W::Tool { .. } | W::Io { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, e.to_string())
    }
}
```

In `crates/balerix-server/src/plugin_api.rs`, add the three routes to `router()`:

```rust
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/workspace/diff",
            get(workspace_diff),
        )
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/workspace/file",
            get(workspace_file),
        )
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/workspace/tree",
            get(workspace_tree),
        )
```

and, after `post_action`, the handlers (imports: `balerix_api::{Capability, KvKeys, PluginAction, WorkspaceDiff, WorkspaceTree}`):

```rust
/// The plugin, active for `agent`, that a workspace route serves (Spec C
/// §2.2): the `workspace` capability, then the pair.
async fn workspace_caller(
    state: &AppState,
    headers: &HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
) -> Result<AgentId, ApiError> {
    let plugin = caller(state, headers, Capability::Workspace).await?;
    let Path((f, c, a)) = path.map_err(|e| ApiError::new(e.status(), e.body_text()))?;
    let agent: AgentId = format!("{f}/{c}/{a}")
        .parse()
        .map_err(|_: balerix_core::NameError| ApiError::from(DaemonError::NotFound))?;
    if !state.daemon.registry().is_active(&agent, &plugin) {
        return Err(PluginError::NotActive(agent.to_string()).into());
    }
    Ok(agent)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PathQuery {
    path: String,
}

fn path_query(q: Result<Query<PathQuery>, QueryRejection>) -> Result<String, ApiError> {
    q.map(|Query(q)| q.path)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))
}

async fn workspace_diff(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
) -> Result<Json<WorkspaceDiff>, ApiError> {
    let agent = workspace_caller(&state, &headers, path).await?;
    let base_ref = state
        .daemon
        .base_ref(&agent)
        .await
        .ok_or(DaemonError::NotFound)?;
    let ws = state.daemon.workspace();
    let id = agent.clone();
    let diff = tokio::task::spawn_blocking(move || ws.diff(&id, &base_ref))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(diff))
}

async fn workspace_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
    q: Result<Query<PathQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let agent = workspace_caller(&state, &headers, path).await?;
    let rel = path_query(q)?;
    let ws = state.daemon.workspace();
    let bytes = tokio::task::spawn_blocking(move || ws.read_file(&agent, &rel))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(([(CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn workspace_tree(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
    q: Result<Query<PathQuery>, QueryRejection>,
) -> Result<Json<WorkspaceTree>, ApiError> {
    let agent = workspace_caller(&state, &headers, path).await?;
    let rel = path_query(q)?;
    let ws = state.daemon.workspace();
    let tree = tokio::task::spawn_blocking(move || ws.list_dir(&agent, &rel))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(tree))
}
```

Run: `mise x -- cargo test -p balerix-server --test workspace_it` (raw assertions), then `mise x -- cargo test -p balerix-server` and clippy for the workspace.
Expected: PASS; `events_it` still passes (web still lacks `kv`).

- [ ] **Step 4: Commit**

```bash
git add crates/balerix-server crates/balerix/src/commands/serve.rs
git commit -q -F - > "$SCRATCH/commit.log" 2>&1 <<'EOF'
Serve the workspace routes from the daemon

Spec C §2.2: `GET /v1/plugin-host/agents/{id}/workspace/{diff,file,tree}`
behind the plugin's bearer, the new `workspace` capability and an active
pair, answered by the `WorkspaceReader` port the binary wires to the
runtime. The crew's `ref` becomes the diff's base; the two 404s are told
apart by their text; an `Io` error is logged and never sent.

Claude-Session: https://claude.ai/code/session_01XVWfZKBMR5cciaB4H5cXz5
EOF
grep -E "Summary|FAIL" "$SCRATCH/commit.log"; git log --oneline -1
```

---

### Task 4: SDK — `Host::workspace_*`, `FakeHost` workspaces and `fail_actions`, the fixtures

**Files:**
- Modify: `crates/balerix-plugin-sdk/src/host.rs` (after `kv_list`)
- Modify: `crates/balerix-plugin-sdk/src/testing.rs` (`Inner`, `FakeHost::{start, set_workspace, fail_actions}`, `router`, `post_action`, three handlers)
- Create: `docs/plugin-protocol/workspace-diff.json`, `workspace-file.json`, `workspace-tree.json`
- Modify: `crates/balerix-plugin-sdk/tests/conformance.rs` (count, the replay)
- Modify: `crates/balerix-server/tests/workspace_it.rs` (the `Host` calls of Task 3 step 2)
- Modify: `docs/plugin-protocol.md` §3 (rows, a Workspace paragraph), §6 (count)

**Interfaces:**
- Produces:
  ```rust
  // balerix_plugin_sdk::Host
  pub async fn workspace_diff(&self, agent: &str) -> Result<WorkspaceDiff, SdkError>;           // 30 s timeout
  pub async fn workspace_file(&self, agent: &str, path: &str) -> Result<Option<Vec<u8>>, SdkError>;  // None on 404 "no such path" only
  pub async fn workspace_tree(&self, agent: &str, path: &str) -> Result<WorkspaceTree, SdkError>;
  // balerix_plugin_sdk::testing::FakeHost
  pub fn set_workspace(&self, agent: &str, diff: WorkspaceDiff, files: BTreeMap<String, Vec<u8>>);
  /// While `Some`, every `POST agents/…/actions` answers 500 `{ error: <message> }`.
  pub fn fail_actions(&self, message: Option<&str>);
  ```

- [ ] **Step 1: The fixtures**

`docs/plugin-protocol/workspace-diff.json`:

```json
{
  "route": "GET /v1/plugin-host/agents/payments/backend/bob/workspace/diff",
  "direction": "plugin-to-daemon",
  "request": null,
  "status": 200,
  "response": {
    "base_ref": "origin/main",
    "merge_base": "3f9c2a1d5b7e8c0a4f6d2e1b9a8c7d6e5f4a3b2c",
    "head": "b7e0d44a1c2f3e4d5a6b7c8d9e0f1a2b3c4d5e6f",
    "files": [
      {
        "path": "src/lib.rs",
        "old_path": null,
        "status": "modified",
        "uncommitted": true,
        "binary": false,
        "patch": "diff --git a/src/lib.rs b/src/lib.rs\nindex 1111111..2222222 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-fn main() {}\n+fn main() { run(); }\n",
        "truncated": false
      },
      {
        "path": "COPYING",
        "old_path": "LICENSE",
        "status": "renamed",
        "uncommitted": false,
        "binary": false,
        "patch": "diff --git a/LICENSE b/COPYING\nsimilarity index 100%\nrename from LICENSE\nrename to COPYING\n",
        "truncated": false
      }
    ],
    "truncated": false
  }
}
```

`docs/plugin-protocol/workspace-file.json` (`raw` is `fn main() {}\n`):

```json
{
  "route": "GET /v1/plugin-host/agents/payments/backend/bob/workspace/file?path=src/lib.rs",
  "direction": "plugin-to-daemon",
  "request": null,
  "status": 200,
  "raw": "Zm4gbWFpbigpIHt9Cg=="
}
```

`docs/plugin-protocol/workspace-tree.json`:

```json
{
  "route": "GET /v1/plugin-host/agents/payments/backend/bob/workspace/tree?path=src",
  "direction": "plugin-to-daemon",
  "request": null,
  "status": 200,
  "response": { "path": "src", "entries": [ { "name": "lib.rs", "kind": "file", "size": 13 } ] }
}
```

- [ ] **Step 2: Write the failing conformance additions**

In `crates/balerix-plugin-sdk/tests/conformance.rs`, change the count assertion (`out.len(), 18`) to `21`, and append to `the_host_sends_every_plugin_to_daemon_fixture_and_reads_the_answer`, before its closing brace:

```rust
    let diff: WorkspaceDiff =
        serde_json::from_value(fx["workspace-diff"]["response"].clone()).unwrap();
    let file = b64(fx["workspace-file"]["raw"].as_str().unwrap());
    fake.set_workspace(
        "payments/backend/bob",
        diff.clone(),
        BTreeMap::from([("src/lib.rs".to_string(), file.clone())]),
    );
    assert_eq!(
        serde_json::to_value(host.workspace_diff("payments/backend/bob").await.unwrap()).unwrap(),
        fx["workspace-diff"]["response"]
    );
    assert_eq!(
        host.workspace_file("payments/backend/bob", "src/lib.rs")
            .await
            .unwrap(),
        Some(file)
    );
    assert_eq!(
        host.workspace_file("payments/backend/bob", "nope")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        serde_json::to_value(host.workspace_tree("payments/backend/bob", "src").await.unwrap())
            .unwrap(),
        fx["workspace-tree"]["response"]
    );
    // an agent with no workspace is the other 404, surfaced as an error
    let e = host.workspace_diff("payments/backend/nobody").await.unwrap_err();
    assert_eq!(
        e.to_string(),
        "daemon: HTTP 404: no workspace for agent payments/backend/nobody"
    );
    let e = host
        .workspace_file("payments/backend/nobody", "x")
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no workspace"), "{e}");
```

(`WorkspaceDiff` joins the `balerix_api` import; `BTreeMap` is already imported.)

Run: `mise x -- cargo test -p balerix-plugin-sdk --test conformance`
Expected: compile error (no `workspace_diff`).

- [ ] **Step 3: `Host` methods**

In `crates/balerix-plugin-sdk/src/host.rs`, add `WorkspaceDiff, WorkspaceTree` to the `balerix_api` import, a constant beside `TIMEOUT`:

```rust
/// `workspace_diff` alone: a first diff of a large repository can outlast
/// the client's 10 s.
const DIFF_TIMEOUT: Duration = Duration::from_secs(30);
```

and after `kv_list`:

```rust
    /// `GET agents/{id}/workspace/diff` (Spec C §2.2): the agent's
    /// worktree against the crew's base. Needs `workspace` and an active
    /// pair; 404 `no workspace for agent …` before the worktree exists.
    pub async fn workspace_diff(&self, agent: &str) -> Result<WorkspaceDiff, SdkError> {
        self.json(
            self.http
                .get(self.url(&format!("agents/{agent}/workspace/diff")))
                .timeout(DIFF_TIMEOUT),
        )
        .await
    }

    /// `GET agents/{id}/workspace/file?path=`: the bytes, `None` when the
    /// worktree exists and the path does not (`no such path`); every
    /// other refusal — no worktree, not a file, over 1 MiB, a bad path —
    /// is the daemon's status and message.
    pub async fn workspace_file(&self, agent: &str, path: &str) -> Result<Option<Vec<u8>>, SdkError> {
        let (status, bytes) = self
            .send(self.http.get(self.url(&format!(
                "agents/{agent}/workspace/file?path={}",
                urlencode(path)
            ))))
            .await?;
        match status {
            200..=299 => Ok(Some(bytes)),
            404 => match Self::status_error(status, &bytes) {
                SdkError::Status { message, .. } if message == "no such path" => Ok(None),
                e => Err(e),
            },
            _ => Err(Self::status_error(status, &bytes)),
        }
    }

    /// `GET agents/{id}/workspace/tree?path=`: one directory listing; the
    /// empty path is the root.
    pub async fn workspace_tree(&self, agent: &str, path: &str) -> Result<WorkspaceTree, SdkError> {
        self.json(self.http.get(self.url(&format!(
            "agents/{agent}/workspace/tree?path={}",
            urlencode(path)
        ))))
        .await
    }
```

- [ ] **Step 4: `FakeHost`**

In `crates/balerix-plugin-sdk/src/testing.rs`: add `EntryKind, TreeEntry, WorkspaceDiff, WorkspaceTree` to the `balerix_api` import and `use axum::extract::Query;`. `Inner` gains

```rust
    workspaces: Mutex<BTreeMap<String, (WorkspaceDiff, BTreeMap<String, Vec<u8>>)>>,
    /// `fail_actions`: while set, `POST agents/…/actions` answers 500.
    action_failure: Mutex<Option<String>>,
```

initialised in `start` as `workspaces: Mutex::new(BTreeMap::new()), action_failure: Mutex::new(None),`. Methods on `FakeHost`:

```rust
    /// What the three workspace routes answer for `agent`: the diff as
    /// given (its `base_ref` kept), and `file`/`tree` from a flat map of
    /// relative path → bytes. An agent never set has no workspace.
    pub fn set_workspace(&self, agent: &str, diff: WorkspaceDiff, files: BTreeMap<String, Vec<u8>>) {
        self.inner
            .workspaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(agent.to_string(), (diff, files));
    }

    /// While `Some`, every `POST agents/…/actions` answers 500 with that
    /// message — the daemon's runner failing.
    pub fn fail_actions(&self, message: Option<&str>) {
        *self
            .inner
            .action_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = message.map(str::to_string);
    }
```

In `post_action`, after the bearer check and before recording, add:

```rust
    if let Some(message) = inner
        .action_failure
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return error(StatusCode::INTERNAL_SERVER_ERROR, message);
    }
```

Routes in `router`:

```rust
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/workspace/diff",
            get(workspace_diff),
        )
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/workspace/file",
            get(workspace_file),
        )
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/workspace/tree",
            get(workspace_tree),
        )
```

Handlers (after `attach`):

```rust
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PathQuery {
    path: String,
}

/// The entries directly under `path` in a flat path map; `None` when
/// nothing lives there (the same shape `balerix_core::fakes` derives).
fn tree_of(files: &BTreeMap<String, Vec<u8>>, path: &str) -> Option<WorkspaceTree> {
    let prefix = if path.is_empty() {
        String::new()
    } else {
        format!("{path}/")
    };
    let mut entries: BTreeMap<String, TreeEntry> = BTreeMap::new();
    for (key, bytes) in files {
        let Some(rest) = key.strip_prefix(&prefix) else {
            continue;
        };
        match rest.split_once('/') {
            Some((dir, _)) => {
                entries.entry(dir.to_string()).or_insert(TreeEntry {
                    name: dir.to_string(),
                    kind: EntryKind::Dir,
                    size: None,
                });
            }
            None => {
                entries.insert(
                    rest.to_string(),
                    TreeEntry {
                        name: rest.to_string(),
                        kind: EntryKind::File,
                        size: Some(bytes.len() as u64),
                    },
                );
            }
        }
    }
    if entries.is_empty() && !path.is_empty() {
        return None;
    }
    Some(WorkspaceTree {
        path: path.to_string(),
        entries: entries.into_values().collect(),
    })
}

fn with_workspace<T: IntoResponse>(
    inner: &Inner,
    headers: &HeaderMap,
    (fleet, crew, agent): (String, String, String),
    f: impl FnOnce(&WorkspaceDiff, &BTreeMap<String, Vec<u8>>) -> T,
) -> Response {
    if let Some(resp) = unauthorized(inner, headers) {
        return resp;
    }
    let id = format!("{fleet}/{crew}/{agent}");
    let ws = inner.workspaces.lock().unwrap_or_else(|e| e.into_inner());
    match ws.get(&id) {
        Some((diff, files)) => f(diff, files).into_response(),
        None => error(StatusCode::NOT_FOUND, format!("no workspace for agent {id}")),
    }
}

async fn workspace_diff(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<(String, String, String)>,
) -> Response {
    with_workspace(&inner, &headers, id, |diff, _| Json(diff.clone()))
}

async fn workspace_file(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<(String, String, String)>,
    Query(q): Query<PathQuery>,
) -> Response {
    with_workspace(&inner, &headers, id, |_, files| match files.get(&q.path) {
        Some(bytes) => {
            ([(CONTENT_TYPE, "application/octet-stream")], bytes.clone()).into_response()
        }
        None => error(StatusCode::NOT_FOUND, "no such path"),
    })
}

async fn workspace_tree(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<(String, String, String)>,
    Query(q): Query<PathQuery>,
) -> Response {
    with_workspace(&inner, &headers, id, |_, files| {
        if files.contains_key(&q.path) {
            return error(StatusCode::BAD_REQUEST, "not a directory");
        }
        match tree_of(files, &q.path) {
            Some(tree) => Json(tree).into_response(),
            None => error(StatusCode::NOT_FOUND, "no such path"),
        }
    })
}
```

Run: `mise x -- cargo test -p balerix-plugin-sdk`
Expected: PASS (conformance's count and replay included).

- [ ] **Step 5: Close Task 3's test and the protocol doc**

Add the `web.workspace_diff` / `workspace_file` / `workspace_tree` / `flow.workspace_diff` blocks from Task 3 step 2 into `crates/balerix-server/tests/workspace_it.rs` (they are in the listing there) and run `mise x -- cargo test -p balerix-server --test workspace_it`. Expected: PASS.

In `docs/plugin-protocol.md` §3: the capability list in the paragraph becomes "(`fleets`, `actions`, `attach`, `kv`, `workspace`)"; add three rows to the table after the `attach` rows:

```
| `GET agents/{fleet}/{crew}/{agent}/workspace/diff` | `workspace` | — | `WorkspaceDiff` | 200 | `workspace-diff.json` |
| `GET agents/…/workspace/file?path=<rel>` | `workspace` | — | raw bytes | 200 | `workspace-file.json` |
| `GET agents/…/workspace/tree?path=<rel>` | `workspace` | — | `{ path, entries }` | 200 | `workspace-tree.json` |
| `GET agents/…/workspace/*`, agent not active for this plugin | `workspace` | — | `{ "error": "plugin is not active for agent <id>" }` | 404 | (as for actions) |
| `GET agents/…/workspace/*`, no worktree yet | `workspace` | — | `{ "error": "no workspace for agent <id>" }` | 404 | (asserted by `workspace_it.rs`, §6) |
```

and, after the **Streams** paragraph, a **Workspace** paragraph:

```
**Workspace** (Spec C §2.2). `diff` is the agent's worktree against the
merge-base with `origin/<crew ref>`: `{ base_ref, merge_base, head, files,
truncated }`, each file `{ path, old_path, status, uncommitted, binary,
patch, truncated }` with `status` one of `added`, `modified`, `deleted`,
`renamed`, `copied`, `typechange`; untracked files are `added` and
`uncommitted`; a patch is unified with three lines of context, empty for a
binary, cut at 256 KiB (`truncated`), and the list stops at 500 files
(top-level `truncated`). `file` answers the bytes of one regular file, 404
`no such path`, 400 `not a regular file` (a symlink is never followed),
413 `file larger than 1 MiB`. `tree` lists one directory (`{ name, kind:
file|dir|symlink|other, size }`, sorted, `.git` never listed, never
recursive); the empty path is the root. `path` is relative, `/`-separated,
at most 4096 bytes, with no empty, `.` or `..` segment, no `\`, no NUL and
no `.git` segment, else 400 `workspace: invalid path: <reason>`. Two 404
texts: `no workspace for agent <id>` (no worktree) and `no such path`.
```

In §6, "eighteen fixtures" becomes "twenty-one fixtures" and the sentence listing what `events_it.rs` asserts gains "and `crates/balerix-server/tests/workspace_it.rs` the workspace routes' 403, 404s, 413 and 400".

- [ ] **Step 6: Commit**

```bash
git add crates/balerix-plugin-sdk crates/balerix-server/tests docs/plugin-protocol docs/plugin-protocol.md
git commit -q -F - > "$SCRATCH/commit.log" 2>&1 <<'EOF'
Add the workspace routes to the SDK, the fake host and the protocol

Spec C §2.3: `Host::{workspace_diff, workspace_file, workspace_tree}` (the
diff with a 30 s timeout; `file` maps only "no such path" to None),
`FakeHost::set_workspace` and `fail_actions`, three fixtures replayed by
the conformance test, and the protocol doc's rows and Workspace
paragraph.

Claude-Session: https://claude.ai/code/session_01XVWfZKBMR5cciaB4H5cXz5
EOF
grep -E "Summary|FAIL" "$SCRATCH/commit.log"; git log --oneline -1
```

---

### Task 5: `send_text` pastes multi-line text through a tmux buffer

**Files:**
- Modify: `crates/balerix-runtime/src/tmux.rs:441-449` (`send_text`)
- Modify: `crates/balerix-runtime/tests/tmux_it.rs` (a new test)

**Interfaces:**
- Consumes: `TmuxRunner::{run, window_target}`.
- Produces: `AgentRunner::send_text` unchanged in signature; a text containing `\n` arrives as one bracketed paste (Spec C §3.3). `FakeRunner::send_text` is untouched.

- [ ] **Step 1: Write the failing integration test**

Append to `crates/balerix-runtime/tests/tmux_it.rs`:

```rust
/// Spec C §3.3: a multi-line `send_text` arrives whole. `send-keys -l`
/// would deliver a literal newline as Ctrl-J to the application; a
/// buffer paste delivers the text as a terminal paste does, `\r` between
/// the lines, which a cooked tty turns back into `\n` for `cat`.
#[test]
fn send_text_with_newlines_arrives_as_one_paste() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("tmux", false));
        return;
    };
    let root = support::temp_root("tmux-paste");
    let socket = format!("balerix-test-paste-{}", std::process::id());
    let _server = KillServer {
        tmux: tools.tmux.clone(),
        socket: socket.clone(),
    };
    let r = TmuxRunner::new(tools.tmux.clone(), socket.clone());
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew = id.crew_ref();
    let agent_dir = root.join("a");
    std::fs::create_dir_all(agent_dir.join("logs")).unwrap();
    let stdin_log = agent_dir.join("stdin.log");
    let script = agent_dir.join("launch.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nexec cat >> {}\n", stdin_log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = LaunchPlan {
        cwd: agent_dir.clone(),
        env: BTreeMap::new(),
        argv: vec![],
        script: script.clone(),
    };
    r.ensure_crew(&crew).unwrap();
    r.ensure_agent(&id, &plan).unwrap();
    wait_for(|| matches!(r.observe(&id.fleet).unwrap().get(&id), Some(ProcessState::Running { .. })));
    // give `cat` a moment to be the pane's foreground process
    std::thread::sleep(Duration::from_millis(300));

    r.send_text(&id, "single", true).unwrap();
    r.send_text(&id, "line one\nline two\n\nline four", true).unwrap();
    wait_for(|| std::fs::read_to_string(&stdin_log).is_ok_and(|s| s.contains("line four")));
    let got = std::fs::read_to_string(&stdin_log).unwrap();
    assert_eq!(got, "single\nline one\nline two\n\nline four\n", "{got:?}");
    // the paste buffer was deleted afterwards
    let buffers = std::process::Command::new(&tools.tmux)
        .args(["-L", &socket, "list-buffers"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&buffers.stdout).contains("balerix-send-"),
        "{}",
        String::from_utf8_lossy(&buffers.stdout)
    );
    r.stop_crew(&crew).unwrap();
}
```

Run: `mise x -- cargo test -p balerix-runtime --test tmux_it send_text_with_newlines`
Expected: with `send-keys -l`, the pane's `cat` (cooked mode) receives the newlines as `\n`, so the content assertion may pass by accident; the buffer-deletion assertion is vacuous until the buffer exists. The implementation below is what Claude Code (raw mode, bracketed paste on) needs; the by-hand `verify-claude` check in Task 9 is the verdict for that (Spec C §8).

- [ ] **Step 2: The implementation**

In `crates/balerix-runtime/src/tmux.rs`, add `use std::sync::atomic::{AtomicU64, Ordering};` and, near the top, `static SEND_SEQ: AtomicU64 = AtomicU64::new(0);`. Replace `send_text`:

```rust
    /// One line goes through `send-keys -l`. Text with a newline goes
    /// through a named buffer and `paste-buffer -p` (Spec C §3.3): `-p`
    /// wraps it in bracketed-paste markers when the application asked for
    /// them, which is how Claude Code takes a multi-line paste as one
    /// message; `-d` deletes the buffer. Buffer names are unique per
    /// process so two concurrent sends cannot swap texts.
    fn send_text(&self, agent: &AgentId, text: &str, submit: bool) -> Result<(), RunnerError> {
        let id = agent.to_string();
        let target = Self::window_target(agent);
        if text.contains('\n') {
            let buffer = format!(
                "balerix-send-{}-{}",
                std::process::id(),
                SEND_SEQ.fetch_add(1, Ordering::Relaxed)
            );
            self.run(&id, &["set-buffer", "-b", &buffer, "--", text])?;
            self.run(
                &id,
                &["paste-buffer", "-p", "-d", "-b", &buffer, "-t", &target],
            )?;
        } else {
            self.run(&id, &["send-keys", "-t", &target, "-l", "--", text])?;
        }
        if submit {
            self.run(&id, &["send-keys", "-t", &target, "Enter"])?;
        }
        Ok(())
    }
```

Run: `mise x -- cargo test -p balerix-runtime --test tmux_it` and clippy.
Expected: PASS, all tmux tests.

- [ ] **Step 3: Commit**

```bash
git add crates/balerix-runtime
git commit -q -F - > "$SCRATCH/commit.log" 2>&1 <<'EOF'
Paste multi-line send_text through a tmux buffer

Spec C §3.3: `send-keys -l` hands a literal newline to the application as
Ctrl-J, which Claude Code may or may not read as a newline depending on
the version. A named buffer and `paste-buffer -p -d` deliver the text as
a terminal paste, bracketed when the application asked for it, and drop
the buffer afterwards. Single lines are unchanged.

Claude-Session: https://claude.ai/code/session_01XVWfZKBMR5cciaB4H5cXz5
EOF
grep -E "Summary|FAIL" "$SCRATCH/commit.log"; git log --oneline -1
```

---

### Task 6: web — the event buffer, `observe`, `events.json`

**Files:**
- Modify: `crates/balerix-plugin-web/package/balerix-plugin.yaml`
- Modify: `crates/balerix-plugin-web/src/state.rs`
- Modify: `crates/balerix-plugin-web/src/plugin.rs`
- Modify: `crates/balerix-plugin-web/src/routes.rs` (one route)
- Modify: `crates/balerix-plugin-web/src/lib.rs` (re-exports)
- Modify: `crates/balerix-plugin-web/tests/plugin_it.rs`

**Interfaces:**
- Consumes: `balerix_api::{HookEvent, Timestamp, AgentPhase}`, `Plugin::observe`, `Metrics::{int_counter, int_counter_vec}`.
- Produces:
  ```rust
  // balerix_plugin_web::state
  pub const EVENT_BUFFER: usize = 500;
  pub const PAYLOAD_LIMIT: usize = 4096;
  pub struct Entry { pub seq: u64, pub at: Timestamp, pub name: String, pub summary: String,
                     pub payload: Value, pub payload_truncated: bool }   // Serialize/Deserialize
  pub struct Events { pub phase: AgentPhase, pub events: Vec<Entry> }    // the events.json body
  impl Cache {
      /// Appends for an enabled agent and returns its seq; `None` (dropped) otherwise.
      pub fn push_event(&self, agent: &str, at: Timestamp, name: &str, summary: String, payload: Value) -> Option<u64>;
      pub fn events_after(&self, agent: &str, after: u64) -> Events;
      pub fn phase_of(&self, agent: &str) -> AgentPhase;
  }
  pub fn summarize(name: &str, payload: &Value) -> String;
  pub fn cut_payload(payload: Value) -> (Value, bool);
  pub fn now() -> Timestamp;                                              // seconds since the epoch
  // balerix_plugin_web::plugin::Shared { …, pub reviews_total: IntCounterVec, pub review_comments_total: IntCounter, pub events_buffered_total: IntCounter }
  // route: GET /agents/{f}/{c}/{a}/events.json?after=<seq>  → Events; 404 for an agent that is not enabled
  ```

- [ ] **Step 1: The manifest**

`crates/balerix-plugin-web/package/balerix-plugin.yaml` becomes:

```yaml
apiVersion: balerix/v1
kind: Plugin
name: web
version: 0.2.0
protocol: 1
start: serve
# phases come from fleets/watch (plugins spec §18.5); every event is
# observed for the review page's activity column (Spec C §4.1)
hooks:
  observe: [SessionStart, SessionEnd, UserPromptSubmit, PreToolUse, PostToolUse, Notification, Stop, SubagentStop, PreCompact]
# actions: the review is a send_text; workspace: the diff, file and tree routes
needs: [fleets, attach, actions, workspace]
routes: true
```

- [ ] **Step 2: Write the failing unit tests**

In `crates/balerix-plugin-web/src/state.rs` tests add:

```rust
    #[test]
    fn events_are_buffered_per_enabled_agent_with_a_running_seq() {
        use serde_json::json;
        let c = Cache::new();
        assert_eq!(
            c.push_event("f/c/a", Timestamp(1), "Stop", "turn ended".into(), json!({})),
            None,
            "not enabled: dropped"
        );
        c.set_enabled("f/c/a", true);
        for i in 1..=(EVENT_BUFFER as u64 + 3) {
            let seq = c
                .push_event("f/c/a", Timestamp(i), "PreToolUse", format!("Bash: cmd{i}"), json!({ "i": i }))
                .unwrap();
            assert_eq!(seq, i);
        }
        let all = c.events_after("f/c/a", 0);
        assert_eq!(all.phase, AgentPhase::Pending);
        assert_eq!(all.events.len(), EVENT_BUFFER, "the oldest three were evicted");
        assert_eq!(all.events[0].seq, 4);
        assert_eq!(all.events.last().unwrap().seq, EVENT_BUFFER as u64 + 3);
        let tail = c.events_after("f/c/a", EVENT_BUFFER as u64 + 1);
        assert_eq!(
            tail.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![EVENT_BUFFER as u64 + 2, EVENT_BUFFER as u64 + 3]
        );
        assert!(c.events_after("f/c/a", u64::MAX).events.is_empty());
        assert!(c.events_after("f/c/zzz", 0).events.is_empty(), "unknown agent: empty, not a panic");
        let e = &tail.events[0];
        assert_eq!(
            serde_json::to_value(e).unwrap(),
            json!({ "seq": e.seq, "at": e.at.0, "name": "PreToolUse", "summary": format!("Bash: cmd{}", e.seq),
                    "payload": { "i": e.seq }, "payload_truncated": false })
        );
        c.remove("f/c/a");
        assert!(c.events_after("f/c/a", 0).events.is_empty(), "deactivate drops the buffer");
    }

    #[test]
    fn summaries_follow_the_hook_name_and_payloads_are_cut() {
        use serde_json::json;
        assert_eq!(
            summarize("PreToolUse", &json!({ "tool_name": "Bash", "tool_input": { "command": "cargo test" } })),
            "Bash: cargo test"
        );
        assert_eq!(
            summarize("PostToolUse", &json!({ "tool_name": "Edit", "tool_input": { "file_path": "src/lib.rs" } })),
            "Edit src/lib.rs"
        );
        assert_eq!(summarize("PreToolUse", &json!({ "tool_name": "WebFetch" })), "WebFetch");
        assert_eq!(summarize("PreToolUse", &json!({})), "PreToolUse");
        assert_eq!(summarize("Notification", &json!({ "message": "needs input" })), "needs input");
        assert_eq!(
            summarize("UserPromptSubmit", &json!({ "prompt": "first line\nsecond" })),
            "first line"
        );
        assert_eq!(summarize("Stop", &json!({})), "turn ended");
        assert_eq!(summarize("SubagentStop", &json!({})), "subagent ended");
        assert_eq!(summarize("SessionStart", &json!({ "source": "startup" })), "SessionStart");
        assert_eq!(summarize("Whatever", &json!(null)), "Whatever");
        let long = "x".repeat(300);
        let s = summarize("Notification", &json!({ "message": long }));
        assert_eq!(s.chars().count(), 200);
        let (v, cut) = cut_payload(json!({ "small": 1 }));
        assert_eq!((v, cut), (json!({ "small": 1 }), false));
        let big = json!({ "blob": "y".repeat(PAYLOAD_LIMIT) });
        let (v, cut) = cut_payload(big);
        assert!(cut);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["head"].as_str().unwrap().len(), PAYLOAD_LIMIT);
        assert!(now().0 > 1_700_000_000);
    }
```

Run: `mise x -- cargo test -p balerix-plugin-web --lib state`
Expected: compile error.

- [ ] **Step 3: The buffer**

In `crates/balerix-plugin-web/src/state.rs`: imports become

```rust
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Mutex, MutexGuard};

use balerix_api::{AgentPhase, FleetRecord, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::Value;
```

and add:

```rust
/// Events kept per agent (Spec C §4.1, PC-9).
pub const EVENT_BUFFER: usize = 500;
/// A payload's serialized size beyond which only its head is kept.
pub const PAYLOAD_LIMIT: usize = 4096;

/// One entry of the activity column: a hook event, or the synthetic
/// `review_sent` divider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Per agent, from 1, never reused.
    pub seq: u64,
    pub at: Timestamp,
    pub name: String,
    pub summary: String,
    pub payload: Value,
    #[serde(default)]
    pub payload_truncated: bool,
}

/// The `events.json` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Events {
    pub phase: AgentPhase,
    pub events: Vec<Entry>,
}

#[derive(Default)]
struct EventLog {
    next_seq: u64,
    entries: VecDeque<Entry>,
}

/// Seconds since the epoch, for the synthetic entries the plugin itself
/// appends (hook events carry the daemon's `received_at`).
pub fn now() -> Timestamp {
    Timestamp(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    )
}

fn cut(s: &str, chars: usize) -> String {
    s.chars().take(chars).collect()
}

/// One line per event for the column (Spec C §4.3's table).
pub fn summarize(name: &str, payload: &Value) -> String {
    let text = |v: &Value| v.as_str().map(str::to_string);
    let line = match name {
        "PreToolUse" | "PostToolUse" => {
            let tool = text(&payload["tool_name"]);
            let input = &payload["tool_input"];
            match tool.as_deref() {
                Some("Bash") => text(&input["command"]).map(|c| format!("Bash: {c}")).or(tool),
                Some(t @ ("Edit" | "Write" | "Read" | "MultiEdit")) => {
                    text(&input["file_path"]).map(|p| format!("{t} {p}")).or(tool)
                }
                Some(_) => tool,
                None => None,
            }
        }
        "Notification" => text(&payload["message"]),
        "UserPromptSubmit" => text(&payload["prompt"]).map(|p| p.lines().next().unwrap_or("").to_string()),
        "Stop" => Some("turn ended".into()),
        "SubagentStop" => Some("subagent ended".into()),
        _ => None,
    };
    cut(&line.unwrap_or_else(|| name.to_string()), 200)
}

/// A payload over `PAYLOAD_LIMIT` serialized bytes becomes
/// `{ "truncated": true, "head": <first PAYLOAD_LIMIT bytes> }`.
pub fn cut_payload(payload: Value) -> (Value, bool) {
    let text = payload.to_string();
    if text.len() <= PAYLOAD_LIMIT {
        return (payload, false);
    }
    let mut end = PAYLOAD_LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (
        serde_json::json!({ "truncated": true, "head": &text[..end] }),
        true,
    )
}
```

`Inner` gains `events: BTreeMap<String, EventLog>,`; `remove` also does `i.events.remove(agent);`; `set_enabled(agent, false)` leaves the buffer (the pair is still active — a re-enable keeps its history). New methods on `Cache`:

```rust
    /// Appends one entry for an enabled agent, evicting the oldest past
    /// `EVENT_BUFFER`; `None` when the agent is not enabled (dropped).
    pub fn push_event(
        &self,
        agent: &str,
        at: Timestamp,
        name: &str,
        summary: String,
        payload: Value,
    ) -> Option<u64> {
        let mut i = self.lock();
        if !i.enabled.contains(agent) {
            return None;
        }
        let log = i.events.entry(agent.to_string()).or_default();
        log.next_seq += 1;
        let seq = log.next_seq;
        let (payload, payload_truncated) = cut_payload(payload);
        log.entries.push_back(Entry {
            seq,
            at,
            name: name.to_string(),
            summary,
            payload,
            payload_truncated,
        });
        while log.entries.len() > EVENT_BUFFER {
            log.entries.pop_front();
        }
        Some(seq)
    }

    /// The entries with `seq > after`, oldest first, and the agent's phase.
    pub fn events_after(&self, agent: &str, after: u64) -> Events {
        let i = self.lock();
        let events = i
            .events
            .get(agent)
            .map(|log| log.entries.iter().filter(|e| e.seq > after).cloned().collect())
            .unwrap_or_default();
        Events {
            phase: phase_in(&i.fleets, agent),
            events,
        }
    }

    pub fn phase_of(&self, agent: &str) -> AgentPhase {
        phase_in(&self.lock().fleets, agent)
    }
```

with a helper `rows_of` can use too:

```rust
fn phase_in(fleets: &[FleetRecord], id: &str) -> AgentPhase {
    fleets
        .iter()
        .find_map(|f| f.status.agents.get(id))
        .map_or(AgentPhase::Pending, |s| s.phase)
}
```

Re-export from `lib.rs`: `pub use state::{AgentRow, Cache, EVENT_BUFFER, Entry, Events, PAYLOAD_LIMIT, cut_payload, now, rows_of, summarize};`.

The buffer's property (Spec C §7): add `proptest = { workspace = true }` to `crates/balerix-plugin-web/Cargo.toml`'s `[dev-dependencies]` (the workspace already pins it; no new workspace dependency) and, in `state.rs`'s `mod tests`:

```rust
    mod props {
        use super::super::*;
        use proptest::prelude::*;

        proptest! {
            /// `events_after(after)` is exactly the kept entries with
            /// `seq > after`, in order, for any number of pushes.
            #[test]
            fn events_after_returns_exactly_the_entries_above_after(
                n in 0usize..(EVENT_BUFFER + 50),
                after in 0u64..600,
            ) {
                let c = Cache::new();
                c.set_enabled("f/c/a", true);
                for i in 0..n {
                    c.push_event("f/c/a", Timestamp(i as u64), "Stop", "turn ended".into(), Value::Null);
                }
                let got: Vec<u64> = c.events_after("f/c/a", after).events.iter().map(|e| e.seq).collect();
                let first_kept = (n as u64).saturating_sub(EVENT_BUFFER as u64) + 1;
                let want: Vec<u64> = (1..=n as u64).filter(|s| *s > after && *s >= first_kept).collect();
                prop_assert_eq!(got, want);
            }
        }
    }
```

Run: `mise x -- cargo test -p balerix-plugin-web --lib state`
Expected: PASS.

- [ ] **Step 4: `observe`, the metrics, the route**

In `crates/balerix-plugin-web/src/plugin.rs`: import `balerix_plugin_sdk::metrics::{IntCounter, IntCounterVec, IntGauge}` and `balerix_api::HookEvent`; `Shared` gains

```rust
    pub reviews_total: IntCounterVec,
    pub review_comments_total: IntCounter,
    pub events_buffered_total: IntCounter,
```

registered in `new` as

```rust
        let reviews_total =
            metrics.int_counter_vec("reviews_total", "Reviews submitted, by outcome", &["outcome"])?;
        let review_comments_total =
            metrics.int_counter("review_comments_total", "Line comments sent in reviews")?;
        let events_buffered_total = metrics.int_counter(
            "events_buffered_total",
            "Hook events appended to an agent's activity buffer",
        )?;
```

and in `impl Plugin for WebPlugin`:

```rust
    /// Every observed event of an enabled agent joins its activity buffer
    /// (Spec C §4.1); events of hidden agents are dropped.
    async fn observe(&self, events: Vec<HookEvent>) {
        for e in events {
            let summary = crate::state::summarize(&e.name, &e.payload);
            if self
                .shared
                .cache
                .push_event(&e.agent, e.received_at, &e.name, summary, e.payload)
                .is_some()
            {
                self.shared.events_buffered_total.inc();
            }
        }
    }
```

In `routes.rs` add `.route("/agents/{fleet}/{crew}/{agent}/events.json", get(events_json))` and:

```rust
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct AfterQuery {
    after: u64,
}

/// The activity column's poll: entries after `after`, and the phase.
async fn events_json(
    State(shared): State<Arc<Shared>>,
    Path((fleet, crew, agent)): Path<(String, String, String)>,
    q: Result<Query<AfterQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let id = format!("{fleet}/{crew}/{agent}");
    if !shared.cache.is_enabled(&id) {
        return (StatusCode::NOT_FOUND, "no such agent").into_response();
    }
    let after = match q {
        Ok(Query(q)) => q.after,
        Err(e) => return (StatusCode::BAD_REQUEST, e.body_text()).into_response(),
    };
    Json(shared.cache.events_after(&id, after)).into_response()
}
```

(`axum::extract::Query` joins the imports.)

- [ ] **Step 5: Write the plugin test**

In `crates/balerix-plugin-web/tests/plugin_it.rs` add (import `event` from `balerix_plugin_sdk::testing`):

```rust
#[tokio::test]
async fn observed_events_feed_the_activity_column_of_enabled_agents() {
    let (fake, _, h, _watch) = world().await;
    h.activate(ALICE, json!({})).await.unwrap();
    h.activate(BOB, json!({ "enabled": false })).await.unwrap();
    fake.set_fleets(vec![fleet(&[(ALICE, AgentPhase::Ready)])]);
    h.observe(vec![
        event(ALICE, "PreToolUse", json!({ "tool_name": "Bash", "tool_input": { "command": "cargo test" } })),
        event(BOB, "Stop", json!({})),
        event(ALICE, "Notification", json!({ "message": "x".repeat(6000) })),
        event(ALICE, "Stop", json!({})),
    ])
    .await;
    let (status, _, body) = h.get_route("/agents/e2e/c/alice/events.json", "/v1/plugins/web").await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["phase"], "ready");
    let events = v["events"].as_array().unwrap();
    assert_eq!(events.len(), 3, "{v}");
    assert_eq!(events[0]["seq"], 1);
    assert_eq!(events[0]["summary"], "Bash: cargo test");
    assert_eq!(events[1]["payload_truncated"], true);
    assert_eq!(events[1]["payload"]["truncated"], true);
    assert_eq!(events[2]["summary"], "turn ended");
    let (status, _, body) = h
        .get_route("/agents/e2e/c/alice/events.json?after=2", "/v1/plugins/web")
        .await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["events"].as_array().unwrap().len(), 1);
    assert_eq!(v["events"][0]["seq"], 3);
    let (status, _, _) = h.get_route("/agents/e2e/c/bob/events.json", "/v1/plugins/web").await;
    assert_eq!(status, 404, "hidden agents have no column and their events were dropped");
    let (status, _, _) = h
        .get_route("/agents/e2e/c/alice/events.json?after=x", "/v1/plugins/web")
        .await;
    assert_eq!(status, 400);
    let text = h.metrics().await;
    assert_eq!(
        metric(&text, "balerix_plugin_web_events_buffered_total", &[]),
        Some(3.0)
    );
}
```

Run: `mise x -- cargo test -p balerix-plugin-web` and clippy.
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/balerix-plugin-web
git commit -q -F - > "$SCRATCH/commit.log" 2>&1 <<'EOF'
Buffer observed hook events per agent in the web plugin

Spec C §4.1: web observes all nine events and keeps the last 500 per
enabled agent (payloads cut at 4 KiB) with a per-agent sequence, served
by `events.json?after=` for the review page's activity column. The
manifest also declares `actions` and `workspace` for the tasks that
follow. Nothing is persisted: the daemon offers no catch-up (PC-9).

Claude-Session: https://claude.ai/code/session_01XVWfZKBMR5cciaB4H5cXz5
EOF
grep -E "Summary|FAIL" "$SCRATCH/commit.log"; git log --oneline -1
```

---

### Task 7: web — the review page, `diff.json`, `file`, the index link

**Files:**
- Modify: `crates/balerix-plugin-web/src/routes.rs`
- Modify: `crates/balerix-plugin-web/tests/plugin_it.rs`

**Interfaces:**
- Consumes: `Host::{workspace_diff, workspace_file}`, `SdkError::Status`.
- Produces:
  ```rust
  // balerix_plugin_web::routes
  pub fn review_html(prefix: &str, id: &str) -> String;
  fn sdk_error(e: SdkError) -> Response;          // Status → that status and message; anything else → 502
  fn enabled_id(shared: &Shared, path: (String, String, String)) -> Result<String, Response>;  // the shared 404
  // routes, each 404 "no such agent" unless enabled:
  //   GET /agents/{id}/review          text/html
  //   GET /agents/{id}/diff.json       the WorkspaceDiff, unchanged
  //   GET /agents/{id}/file?path=      text/plain; charset=utf-8
  // index rows: `<a href="{p}/agents/{id}">{id}</a> <a class="review" href="{p}/agents/{id}/review">review</a>`
  ```

- [ ] **Step 1: Write the failing tests**

In `routes.rs` tests add:

```rust
    #[test]
    fn the_review_page_links_its_routes_through_the_prefix_and_escapes_the_id() {
        let page = review_html("/v1/plugins/web", "f/c/a");
        assert!(page.contains(r#"const prefix = "/v1/plugins/web""#));
        assert!(page.contains(r#"const id = "f/c/a""#));
        assert!(page.contains("/diff.json"), "fetches the diff");
        assert!(page.contains("/events.json?after="), "polls the column");
        assert!(page.contains(r#"href="/v1/plugins/web/agents/f/c/a""#), "back to the terminal");
        assert!(page.contains(r#"id="collapse""#));
        assert!(page.contains("balerix-review/"), "the draft key");
        let page = review_html("/p", "<x>&");
        assert!(page.contains("&lt;x&gt;&amp;") && !page.contains("<x>"));
        assert!(
            page.contains(r#"const id = "\u003cx\u003e\u0026""#),
            "the id reaches the script as a JSON literal with <, > and & escaped: {page}"
        );
        let rows = vec![AgentRow {
            id: "f/c/a".into(),
            phase: AgentPhase::Ready,
            message: String::new(),
        }];
        let index = index_html("/v1/plugins/web", &rows);
        assert!(
            index.contains(r#"<a class="review" href="/v1/plugins/web/agents/f/c/a/review">review</a>"#),
            "{index}"
        );
        assert!(index.contains(r#"prefix + "/agents/" + r.id + "/review""#), "the refresh script too");
    }
```

(`serde_json` leaves `<`, `>` and `&` alone inside strings, so `js_string` in step 3 escapes them as `\u003c`, `\u003e`, `\u0026` — nothing in the id or prefix can close the script element; the assertion checks the escaped form.)

In `tests/plugin_it.rs` add:

```rust
fn sample_diff() -> balerix_api::WorkspaceDiff {
    use balerix_api::{FileDiff, FileStatus, WorkspaceDiff};
    WorkspaceDiff {
        base_ref: "origin/main".into(),
        merge_base: "m".repeat(40),
        head: "3f9c2a1".to_string() + &"0".repeat(33),
        files: vec![FileDiff {
            path: "src/lib.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            uncommitted: true,
            binary: false,
            patch: "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n fn a() {}\n-fn b() {}\n+fn b() { c() }\n".into(),
            truncated: false,
        }],
        truncated: false,
    }
}

#[tokio::test]
async fn the_review_page_and_its_data_routes_pass_the_workspace_through() {
    let (fake, _, h, _watch) = world().await;
    h.activate(ALICE, json!({})).await.unwrap();
    h.activate(BOB, json!({ "enabled": false })).await.unwrap();
    fake.set_workspace(
        ALICE,
        sample_diff(),
        BTreeMap::from([("src/lib.rs".to_string(), b"fn a() {}\nfn b() { c() }\n".to_vec())]),
    );
    let (status, headers, body) = h.get_route("/agents/e2e/c/alice/review", "/v1/plugins/web").await;
    assert_eq!(status, 200);
    assert!(headers.iter().any(|(k, v)| k == "content-type" && v.starts_with("text/html")));
    let page = String::from_utf8(body).unwrap();
    assert!(page.contains(r#"const prefix = "/v1/plugins/web""#));
    assert!(page.contains("e2e/c/alice"));

    let (status, _, body) = h.get_route("/agents/e2e/c/alice/diff.json", "/v1/plugins/web").await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v, serde_json::to_value(sample_diff()).unwrap(), "unchanged");

    let (status, headers, body) = h
        .get_route("/agents/e2e/c/alice/file?path=src/lib.rs", "/v1/plugins/web")
        .await;
    assert_eq!(status, 200);
    assert!(headers.iter().any(|(k, v)| k == "content-type" && v == "text/plain; charset=utf-8"));
    assert_eq!(body, b"fn a() {}\nfn b() { c() }\n");
    let (status, _, body) = h
        .get_route("/agents/e2e/c/alice/file?path=nope", "/v1/plugins/web")
        .await;
    assert_eq!((status, String::from_utf8_lossy(&body).as_ref()), (404, "no such path"));

    // the daemon's refusals cross as they are: bob is hidden here, carol
    // has no workspace at the fake
    for path in ["review", "diff.json", "file?path=x", "events.json"] {
        let (status, _, _) = h.get_route(&format!("/agents/e2e/c/bob/{path}"), "/v1/plugins/web").await;
        assert_eq!(status, 404, "{path}");
    }
    h.activate("e2e/c/carol", json!({})).await.unwrap();
    let (status, _, body) = h.get_route("/agents/e2e/c/carol/diff.json", "/v1/plugins/web").await;
    assert_eq!(
        (status, String::from_utf8_lossy(&body).as_ref()),
        (404, "no workspace for agent e2e/c/carol")
    );
}
```

Run: `mise x -- cargo test -p balerix-plugin-web`
Expected: compile error (no `review_html`).

- [ ] **Step 2: Routes and the error passthrough**

In `routes.rs`, add the routes:

```rust
        .route("/agents/{fleet}/{crew}/{agent}/review", get(review_page))
        .route("/agents/{fleet}/{crew}/{agent}/diff.json", get(diff_json))
        .route("/agents/{fleet}/{crew}/{agent}/file", get(file_text))
```

the import `balerix_plugin_sdk::SdkError`, and the handlers:

```rust
/// A daemon refusal crosses to the browser with its status and text; a
/// transport failure is a 502.
fn sdk_error(e: SdkError) -> Response {
    match e {
        SdkError::Status { status, message } => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            message,
        )
            .into_response(),
        other => (StatusCode::BAD_GATEWAY, other.to_string()).into_response(),
    }
}

/// The agent id for an enabled agent, else the 404 to answer.
fn enabled_id(
    shared: &Shared,
    (fleet, crew, agent): (String, String, String),
) -> Result<String, Response> {
    let id = format!("{fleet}/{crew}/{agent}");
    if shared.cache.is_enabled(&id) {
        Ok(id)
    } else {
        Err((StatusCode::NOT_FOUND, "no such agent").into_response())
    }
}

async fn review_page(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Path(path): Path<(String, String, String)>,
) -> Response {
    match enabled_id(&shared, path) {
        Ok(id) => Html(review_html(&prefix(&headers), &id)).into_response(),
        Err(r) => r,
    }
}

async fn diff_json(
    State(shared): State<Arc<Shared>>,
    Path(path): Path<(String, String, String)>,
) -> Response {
    let id = match enabled_id(&shared, path) {
        Ok(id) => id,
        Err(r) => return r,
    };
    match shared.host.workspace_diff(&id).await {
        Ok(diff) => Json(diff).into_response(),
        Err(e) => sdk_error(e),
    }
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct FileQuery {
    path: String,
}

async fn file_text(
    State(shared): State<Arc<Shared>>,
    Path(path): Path<(String, String, String)>,
    Query(q): Query<FileQuery>,
) -> Response {
    let id = match enabled_id(&shared, path) {
        Ok(id) => id,
        Err(r) => return r,
    };
    match shared.host.workspace_file(&id, &q.path).await {
        Ok(Some(bytes)) => (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            bytes,
        )
            .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such path").into_response(),
        Err(e) => sdk_error(e),
    }
}
```

Rewrite `terminal`, `bridge_route` and `events_json` over `enabled_id` too (same behaviour, one place).

- [ ] **Step 3: The index link and the page**

In `index_html`, the row becomes:

```rust
            "<tr><td><a href=\"{p}/agents/{id}\">{id}</a> <a class=\"review\" href=\"{p}/agents/{id}/review\">review</a></td><td>{phase}</td><td>{msg}</td></tr>\n",
```

and in its script, after `c1.appendChild(a);`:

```js
      const rv = document.createElement("a");
      rv.className = "review";
      rv.href = prefix + "/agents/" + r.id + "/review";
      rv.textContent = "review";
      c1.append(" ", rv);
```

Add `review_html`. The id and prefix reach the script as JSON string literals with `<`, `>` and `&` escaped (so neither can close the script element) and reach the markup through `html_escape`:

```rust
/// A JSON string literal safe inside `<script>`: `serde_json` escapes
/// quotes, backslashes and control characters; `<`, `>` and `&` are
/// escaped here so no value can close the element.
fn js_string(s: &str) -> String {
    serde_json::to_string(s)
        .unwrap_or_else(|_| "\"\"".into())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

/// The review page (Spec C §4.3): the diff with gutter comments on the
/// left, the activity column on the right, the draft in `localStorage`.
/// Everything the script renders goes through `textContent`; the id and
/// prefix reach the script as JSON string literals.
pub fn review_html(prefix: &str, id: &str) -> String {
    let p_json = js_string(prefix);
    let id_json = js_string(id);
    format!(
        r##"<!doctype html>
<html><head><meta charset="utf-8"><title>review {id}</title>
<style>
body{{font:13px system-ui,sans-serif;margin:0;display:flex;flex-direction:column;height:100vh}}
header{{padding:.5rem 1rem;border-bottom:1px solid #ddd;display:flex;gap:1rem;align-items:center}}
main{{flex:1;display:flex;min-height:0}}
#diff{{flex:1;overflow:auto;padding:0 1rem 7rem}}
#side{{width:26rem;border-left:1px solid #ddd;display:flex;flex-direction:column;min-height:0}}
#side.collapsed{{width:2.2rem}}
#side.collapsed #events,#side.collapsed #side-title{{display:none}}
#side-head{{display:flex;align-items:center;padding:.3rem;border-bottom:1px solid #eee}}
#events{{flex:1;overflow:auto;font:12px ui-monospace,monospace;padding:.4rem}}
.ev{{padding:.2rem .3rem;border-bottom:1px solid #eee;cursor:pointer}}
.ev .t{{color:#888;margin-right:.4rem}}
.ev .n{{color:#57606a;margin-right:.4rem}}
.ev pre{{white-space:pre-wrap;margin:.2rem 0 0;color:#555;max-height:16em;overflow:auto}}
.ev.divider{{background:#fff6d5;font-weight:600}}
.file{{margin:1rem 0;border:1px solid #ddd;border-radius:4px}}
.file h3{{margin:0;padding:.4rem .6rem;background:#f6f8fa;font-size:13px;font-weight:600;display:flex;gap:.6rem;align-items:center}}
.badge{{font-weight:normal;color:#b35900}}
.file h3 a{{font-weight:normal;margin-left:auto}}
table.hunk{{border-collapse:collapse;width:100%;font:12px ui-monospace,monospace}}
table.hunk td{{padding:0 .4rem;white-space:pre;vertical-align:top}}
td.ln{{color:#999;text-align:right;width:3em;user-select:none;cursor:pointer}}
td.ln:hover{{background:#dbe9ff}}
tr.add td.code{{background:#e6ffec}} tr.del td.code{{background:#ffebe9}} tr.hdr td{{background:#f1f8ff;color:#57606a}}
tr.comment td{{background:#fff8c5;white-space:normal;padding:.4rem .6rem}}
tr.comment textarea{{width:100%;min-height:4em;box-sizing:border-box}}
#stale{{border:1px solid #f0c36d;background:#fff8e1;padding:.5rem 1rem;margin:1rem 0}}
footer{{position:fixed;bottom:0;left:0;right:0;border-top:1px solid #ddd;background:#fff;padding:.5rem 1rem;display:flex;gap:1rem;align-items:flex-start}}
footer textarea{{flex:1;min-height:3.5em}}
#banner{{margin-top:.3rem}}
</style></head>
<body>
<header><a href="{p}/">agents</a> <strong>{id}</strong> <a href="{p}/agents/{id}">terminal</a> <span id="meta"></span></header>
<main>
<div id="diff"><p id="loading">loading diff...</p></div>
<aside id="side"><div id="side-head"><button id="collapse" title="collapse or expand the activity column">&#8677;</button><span id="side-title" style="margin-left:.5rem">activity &middot; <span id="phase"></span> <span id="unread"></span></span></div><div id="events"><p id="no-events">no events yet; the daemon delivers no history</p></div></aside>
</main>
<footer><textarea id="summary" placeholder="Overall summary (optional)"></textarea><div><div id="count">0 comments</div><button id="reload">Reload diff</button> <button id="send">Send review</button><div id="banner"></div></div></footer>
<script>
const prefix = {p_json};
const id = {id_json};
const key = "balerix-review/" + id;
let diff = null;
let pending = null;
let draft = {{ comments: [], summary: "", collapsed: false }};
try {{ const s = localStorage.getItem(key); if (s) draft = Object.assign(draft, JSON.parse(s)); }} catch (e) {{}}
function save() {{ try {{ localStorage.setItem(key, JSON.stringify(draft)); }} catch (e) {{}} }}
function el(tag, cls, text) {{ const e = document.createElement(tag); if (cls) e.className = cls; if (text !== undefined) e.textContent = text; return e; }}
const anchorOf = (c) => c.path + " " + c.side + " " + c.line + " " + c.text;

function parsePatch(patch) {{
  const hunks = []; let h = null; let o = 0, n = 0;
  for (const raw of patch.split("\n")) {{
    const m = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(raw);
    if (m) {{ o = +m[1]; n = +m[2]; h = {{ header: raw, lines: [] }}; hunks.push(h); continue; }}
    if (!h) continue;
    if (raw.startsWith("+")) h.lines.push({{ kind: "add", side: "new", line: n++, text: raw }});
    else if (raw.startsWith("-")) h.lines.push({{ kind: "del", side: "old", line: o++, text: raw }});
    else if (raw.startsWith(" ")) h.lines.push({{ kind: "ctx", side: "new", line: n++, oldLine: o++, text: raw }});
    else if (raw.startsWith("\\")) h.lines.push({{ kind: "meta", text: raw }});
  }}
  return hunks;
}}

function commentRow(c, editing) {{
  const tr = el("tr", "comment"); const td = el("td"); td.colSpan = 3;
  if (editing) {{
    const ta = el("textarea"); ta.value = c.body || "";
    const ok = el("button", "", "Save comment"); const no = el("button", "", "Cancel");
    ok.onclick = () => {{ if (!ta.value.trim()) return; c.body = ta.value; delete c.editing; if (!draft.comments.includes(c)) draft.comments.push(c); pending = null; save(); render(); }};
    no.onclick = () => {{ delete c.editing; pending = null; render(); }};
    td.append(ta, ok, " ", no);
    setTimeout(() => ta.focus(), 0);
  }} else {{
    const body = el("div", "", c.body); body.style.whiteSpace = "pre-wrap";
    const edit = el("button", "", "Edit"); const del = el("button", "", "Delete");
    edit.onclick = () => {{ c.editing = true; render(); }};
    del.onclick = () => {{ draft.comments = draft.comments.filter((x) => x !== c); save(); render(); }};
    td.append(body, edit, " ", del);
  }}
  tr.appendChild(td); return tr;
}}

function sameLine(c, f, l) {{ return c.path === f.path && c.side === l.side && c.line === l.line && c.text === l.text; }}

function renderFile(f) {{
  const box = el("div", "file");
  const h3 = el("h3");
  h3.append(el("span", "", f.status), el("span", "", f.old_path ? f.old_path + " -> " + f.path : f.path));
  if (f.uncommitted) h3.appendChild(el("span", "badge", "uncommitted"));
  if (f.binary) h3.appendChild(el("span", "badge", "binary"));
  if (f.truncated) h3.appendChild(el("span", "badge", "patch truncated"));
  if (f.status !== "deleted") {{ const a = el("a", "", "view file"); a.href = prefix + "/agents/" + id + "/file?path=" + encodeURIComponent(f.path); a.target = "_blank"; h3.appendChild(a); }}
  box.appendChild(h3);
  if (f.binary) return box;
  const table = el("table", "hunk");
  for (const h of parsePatch(f.patch)) {{
    const hdr = el("tr", "hdr"); const td = el("td", "", h.header); td.colSpan = 3; hdr.appendChild(td); table.appendChild(hdr);
    for (const l of h.lines) {{
      const tr = el("tr", l.kind);
      const oldNo = l.kind === "del" ? l.line : l.kind === "ctx" ? l.oldLine : "";
      const newNo = l.kind === "add" || l.kind === "ctx" ? l.line : "";
      const c1 = el("td", "ln", String(oldNo)); const c2 = el("td", "ln", String(newNo)); const c3 = el("td", "code", l.text);
      if (l.kind !== "meta") {{
        const start = () => {{ pending = {{ path: f.path, side: l.side, line: l.line, text: l.text, body: "" }}; render(); }};
        c1.onclick = start; c2.onclick = start;
      }}
      tr.append(c1, c2, c3); table.appendChild(tr);
      if (l.kind === "meta") continue;
      for (const c of draft.comments.filter((c) => sameLine(c, f, l))) table.appendChild(commentRow(c, !!c.editing));
      if (pending && sameLine(pending, f, l)) table.appendChild(commentRow(pending, true));
    }}
  }}
  box.appendChild(table); return box;
}}

function render() {{
  const root = document.getElementById("diff"); root.replaceChildren();
  if (!diff) {{ root.appendChild(el("p", "", "loading diff...")); return; }}
  document.getElementById("meta").textContent = "against " + diff.base_ref + " at " + diff.head.slice(0, 7) + (diff.truncated ? " (file list truncated)" : "");
  const anchors = new Set();
  for (const f of diff.files) for (const h of parsePatch(f.patch)) for (const l of h.lines) if (l.kind !== "meta") anchors.add(anchorOf({{ path: f.path, side: l.side, line: l.line, text: l.text }}));
  const stale = draft.comments.filter((c) => !anchors.has(anchorOf(c)));
  if (stale.length) {{
    const box = el("div"); box.id = "stale"; box.appendChild(el("strong", "", "no longer in the diff (still sent):"));
    for (const c of stale) {{ const row = el("div", "", c.path + " line " + c.line + " (" + c.side + "): " + c.body + " "); const del = el("button", "", "Delete"); del.onclick = () => {{ draft.comments = draft.comments.filter((x) => x !== c); save(); render(); }}; row.appendChild(del); box.appendChild(row); }}
    root.appendChild(box);
  }}
  if (!diff.files.length) root.appendChild(el("p", "", "no changes against " + diff.base_ref));
  for (const f of diff.files) root.appendChild(renderFile(f));
  document.getElementById("count").textContent = draft.comments.length + " comment" + (draft.comments.length === 1 ? "" : "s");
  document.getElementById("summary").value = draft.summary;
}}

async function loadDiff() {{
  const banner = document.getElementById("banner"); banner.textContent = "";
  try {{
    const r = await fetch(prefix + "/agents/" + id + "/diff.json");
    if (!r.ok) {{ banner.style.color = "#b00"; banner.textContent = "diff: " + (await r.text()); return; }}
    diff = await r.json(); pending = null; render();
  }} catch (e) {{ banner.style.color = "#b00"; banner.textContent = "diff: " + e; }}
}}

async function send() {{
  const banner = document.getElementById("banner"); banner.textContent = "";
  const comments = draft.comments.map((c) => ({{ path: c.path, side: c.side, line: c.line, text: c.text, body: c.body }}));
  const body = {{ head: diff ? diff.head : "", base_ref: diff ? diff.base_ref : "", summary: draft.summary, comments }};
  try {{
    const r = await fetch(prefix + "/agents/" + id + "/review", {{ method: "POST", headers: {{ "content-type": "application/json" }}, body: JSON.stringify(body) }});
    if (r.ok) {{ draft.comments = []; draft.summary = ""; save(); render(); banner.style.color = "#080"; banner.textContent = "review sent"; }}
    else {{ banner.style.color = "#b00"; banner.textContent = "send failed: " + (await r.text()); }}
  }} catch (e) {{ banner.style.color = "#b00"; banner.textContent = "send failed: " + e; }}
}}

let lastSeq = 0, unread = 0;
const side = document.getElementById("side");
function renderEvent(e) {{
  const row = el("div", "ev" + (e.name === "review_sent" ? " divider" : ""));
  row.append(el("span", "t", new Date(e.at * 1000).toLocaleTimeString()), el("span", "n", e.name), el("span", "s", e.summary));
  const pre = el("pre", "", JSON.stringify(e.payload, null, 2) + (e.payload_truncated ? "\n(truncated)" : "")); pre.hidden = true;
  row.appendChild(pre); row.onclick = () => {{ pre.hidden = !pre.hidden; }};
  return row;
}}
async function pollEvents() {{
  try {{
    const r = await (await fetch(prefix + "/agents/" + id + "/events.json?after=" + lastSeq)).json();
    document.getElementById("phase").textContent = r.phase;
    const box = document.getElementById("events");
    const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 24;
    for (const e of r.events) {{ box.appendChild(renderEvent(e)); lastSeq = e.seq; if (side.classList.contains("collapsed")) unread++; }}
    if (lastSeq > 0) document.getElementById("no-events").hidden = true;
    document.getElementById("unread").textContent = unread ? "(" + unread + " new)" : "";
    if (r.events.length && atBottom) box.scrollTop = box.scrollHeight;
  }} catch (e) {{ console.warn("events", e); }}
}}

function applyCollapse() {{ side.classList.toggle("collapsed", !!draft.collapsed); if (!draft.collapsed) {{ unread = 0; document.getElementById("unread").textContent = ""; }} }}
document.getElementById("collapse").onclick = () => {{ draft.collapsed = !draft.collapsed; save(); applyCollapse(); }};
document.getElementById("reload").onclick = loadDiff;
document.getElementById("send").onclick = send;
document.getElementById("summary").oninput = (ev) => {{ draft.summary = ev.target.value; save(); }};
applyCollapse();
render();
loadDiff();
pollEvents();
setInterval(pollEvents, {poll});
</script>
</body></html>
"##,
        p = html_escape(prefix),
        id = html_escape(id),
        poll = INDEX_POLL_MS,
    )
}
```

Note on `format!` and the script: every literal `{`/`}` in the CSS and JavaScript above is doubled; the placeholders are `{p}`, `{id}`, `{poll}` (named) and `{p_json}`, `{id_json}` (captured from the locals). The unit test's `<x>` assertion checks that the script sees the JSON encoding while the markup sees `&lt;x&gt;`.

Run: `mise x -- cargo test -p balerix-plugin-web` and clippy.
Expected: PASS.

- [ ] **Step 4: Look at it once**

Write `review_html("", "f/c/a")` to a file from a throwaway `#[test]` (or `cargo test -- --nocapture` printing it) and open it in a browser: the layout must show the two columns, the footer and the collapse button, with "loading diff..." and "no events yet" (the fetches fail from `file://`; that is fine for a layout check). Click the collapse button twice. Fix CSS in this step, not later; delete the throwaway test.

- [ ] **Step 5: Commit**

```bash
git add crates/balerix-plugin-web
git commit -q -F - > "$SCRATCH/commit.log" 2>&1 <<'EOF'
Add the review page to the web plugin

Spec C §4.2–§4.3: `review` beside each agent on the index; the page
renders the daemon's diff (passed through `diff.json` unchanged) with
old/new line numbers, gutter-click comments kept in localStorage, a
`view file` link per file, and a collapsible activity column polling
`events.json`. Every rendered value goes through textContent; the id and
prefix reach the script as JSON literals.

Claude-Session: https://claude.ai/code/session_01XVWfZKBMR5cciaB4H5cXz5
EOF
grep -E "Summary|FAIL" "$SCRATCH/commit.log"; git log --oneline -1
```

---

### Task 8: web — the submission, the message, the `review_sent` divider

**Files:**
- Create: `crates/balerix-plugin-web/src/review.rs`
- Modify: `crates/balerix-plugin-web/src/lib.rs`, `src/routes.rs`
- Modify: `crates/balerix-plugin-sdk/src/testing.rs` (`Harness::post_route`)
- Modify: `crates/balerix-plugin-web/tests/plugin_it.rs`

**Interfaces:**
- Consumes: `Host::action`, `PluginAction::SendText`, `Cache::push_event`, `state::now`, `balerix_api::check_path`, `FakeHost::{actions_for, fail_actions}`.
- Produces:
  ```rust
  // balerix_plugin_web::review
  pub const MAX_COMMENTS: usize = 200;
  pub const MAX_BODY_BYTES: usize = 64 << 10;     // summary + every comment body
  pub const MAX_TEXT_BYTES: usize = 4096;         // one quoted diff line
  pub enum Side { Old, New }                      // lowercase on the wire; Display "old"/"new"
  pub struct Comment { pub path: String, pub side: Side, pub line: u64, pub text: String, pub body: String }
  pub struct ReviewBody { pub head: String, pub base_ref: String, pub summary: String, pub comments: Vec<Comment> }
  pub fn validate(body: &ReviewBody) -> Result<(), String>;   // Err is the 400 text, field path first
  pub fn render_message(agent: &str, body: &ReviewBody) -> String;
  // balerix_plugin_sdk::testing::Harness
  pub async fn post_route(&self, path: &str, prefix: &str, body: &Value) -> (u16, Vec<u8>);
  // route: POST /agents/{id}/review  → 200 {} | 400 <reason> | 404 "no such agent" | 502 <daemon message>
  ```

- [ ] **Step 1: Write the failing unit tests**

Create `crates/balerix-plugin-web/src/review.rs`:

```rust
//! The review submission (Spec C §4.4): the body the page posts, its
//! limits, and the one message the agent receives through `send_text`.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body() -> ReviewBody {
        serde_json::from_value(json!({
            "head": "3f9c2a1d5b7e8c0a4f6d2e1b9a8c7d6e5f4a3b2c",
            "base_ref": "origin/main",
            "summary": "Looks close. Please address the comments above and run the tests.\n",
            "comments": [
                { "path": "src/lib.rs", "side": "old", "line": 80, "text": "-    // TODO",
                  "body": "Good riddance, but the docs still mention this." },
                { "path": "src/lib.rs", "side": "new", "line": 42, "text": "+    let x = foo();",
                  "body": "This unwrap can panic on an empty list; return the error instead.\n" }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn the_message_is_the_spec_example() {
        assert_eq!(validate(&body()), Ok(()));
        assert_eq!(
            render_message("e2e/c/alice", &body()),
            "Review of e2e/c/alice against origin/main at 3f9c2a1 (2 comments)\n\
             \n\
             src/lib.rs line 42 (new):\n\
             > +    let x = foo();\n\
             This unwrap can panic on an empty list; return the error instead.\n\
             \n\
             src/lib.rs line 80 (old):\n\
             > -    // TODO\n\
             Good riddance, but the docs still mention this.\n\
             \n\
             Overall:\n\
             Looks close. Please address the comments above and run the tests."
        );
        let mut one = body();
        one.comments.truncate(1);
        one.summary.clear();
        one.base_ref.clear();
        let m = render_message("f/c/a", &one);
        assert!(m.starts_with("Review of f/c/a at 3f9c2a1 (1 comment)\n\n"), "{m}");
        assert!(!m.contains("Overall:"));
        assert!(!m.ends_with('\n'));
    }

    #[test]
    fn the_limits_are_enforced_with_a_field_path() {
        let mut b = body();
        b.comments.clear();
        b.summary.clear();
        assert_eq!(validate(&b), Err("nothing to send".to_string()));
        b.summary = "just a summary".into();
        assert_eq!(validate(&b), Ok(()), "a summary alone is a review");
        let mut b = body();
        b.comments[1].path = "../etc/passwd".into();
        assert_eq!(
            validate(&b),
            Err("comments[1].path: workspace: invalid path: \"..\" segment".to_string())
        );
        let mut b = body();
        b.comments[0].body = String::new();
        assert_eq!(validate(&b), Err("comments[0].body: empty".to_string()));
        let mut b = body();
        b.comments[0].text = "x".repeat(MAX_TEXT_BYTES + 1);
        assert_eq!(
            validate(&b),
            Err(format!("comments[0].text: longer than {MAX_TEXT_BYTES} bytes"))
        );
        let mut b = body();
        b.comments = std::iter::repeat_n(b.comments[0].clone(), MAX_COMMENTS + 1).collect();
        assert_eq!(
            validate(&b),
            Err(format!("comments: more than {MAX_COMMENTS}"))
        );
        let mut b = body();
        b.summary = "y".repeat(MAX_BODY_BYTES);
        assert_eq!(
            validate(&b),
            Err(format!("summary and comment bodies: longer than {MAX_BODY_BYTES} bytes together"))
        );
        assert!(
            serde_json::from_value::<ReviewBody>(json!({ "comments": [], "nope": 1 })).is_err(),
            "unknown fields are refused"
        );
        let minimal: ReviewBody = serde_json::from_value(json!({})).unwrap();
        assert!(minimal.comments.is_empty() && minimal.head.is_empty());
        assert_eq!(Side::Old.to_string(), "old");
    }
}
```

Add `pub mod review;` and `pub use review::{Comment, MAX_BODY_BYTES, MAX_COMMENTS, MAX_TEXT_BYTES, ReviewBody, Side, render_message, validate};` to `lib.rs`.

Run: `mise x -- cargo test -p balerix-plugin-web --lib review`
Expected: compile error.

- [ ] **Step 2: The types, the limits, the message**

Above the tests in `review.rs`:

```rust
use balerix_api::check_path;
use serde::Deserialize;

pub const MAX_COMMENTS: usize = 200;
/// `summary` plus every comment `body`, in bytes.
pub const MAX_BODY_BYTES: usize = 64 << 10;
/// One quoted diff line, in bytes.
pub const MAX_TEXT_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Old,
    New,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Side::Old => "old",
            Side::New => "new",
        })
    }
}

/// One line comment: where it was made and the diff line it was made
/// on (`text`, sign included), so the agent can find the place even
/// after the tree moved (PC-6).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Comment {
    pub path: String,
    pub side: Side,
    pub line: u64,
    #[serde(default)]
    pub text: String,
    pub body: String,
}

/// What `POST /agents/{id}/review` takes. Every field defaults so a page
/// that loaded no diff can still send a summary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewBody {
    pub head: String,
    pub base_ref: String,
    pub summary: String,
    pub comments: Vec<Comment>,
}

/// The 400 text, field path first, or `Ok` for a sendable review.
pub fn validate(body: &ReviewBody) -> Result<(), String> {
    if body.comments.is_empty() && body.summary.trim().is_empty() {
        return Err("nothing to send".into());
    }
    if body.comments.len() > MAX_COMMENTS {
        return Err(format!("comments: more than {MAX_COMMENTS}"));
    }
    let mut bytes = body.summary.len();
    for (i, c) in body.comments.iter().enumerate() {
        check_path(&c.path).map_err(|r| format!("comments[{i}].path: workspace: invalid path: {r}"))?;
        if c.path.is_empty() {
            return Err(format!("comments[{i}].path: empty"));
        }
        if c.body.trim().is_empty() {
            return Err(format!("comments[{i}].body: empty"));
        }
        if c.text.len() > MAX_TEXT_BYTES {
            return Err(format!("comments[{i}].text: longer than {MAX_TEXT_BYTES} bytes"));
        }
        bytes += c.body.len();
    }
    if bytes > MAX_BODY_BYTES {
        return Err(format!(
            "summary and comment bodies: longer than {MAX_BODY_BYTES} bytes together"
        ));
    }
    Ok(())
}

/// The one message the agent receives (Spec C §4.4): a header, the
/// comments in file then line order each quoting its line, and the
/// summary as `Overall:` when there is one. No trailing newline: the
/// runner's `submit` adds the Enter.
pub fn render_message(agent: &str, body: &ReviewBody) -> String {
    let mut comments: Vec<&Comment> = body.comments.iter().collect();
    comments.sort_by(|a, b| {
        (a.path.as_str(), a.line, a.side as u8).cmp(&(b.path.as_str(), b.line, b.side as u8))
    });
    let n = comments.len();
    let short: String = body.head.chars().take(7).collect();
    let mut out = format!("Review of {agent}");
    if !body.base_ref.is_empty() {
        out.push_str(&format!(" against {}", body.base_ref));
    }
    if !short.is_empty() {
        out.push_str(&format!(" at {short}"));
    }
    out.push_str(&format!(
        " ({n} comment{})\n",
        if n == 1 { "" } else { "s" }
    ));
    for c in comments {
        out.push_str(&format!(
            "\n{} line {} ({}):\n> {}\n{}\n",
            c.path,
            c.line,
            c.side,
            c.text,
            c.body.trim_end()
        ));
    }
    let summary = body.summary.trim();
    if !summary.is_empty() {
        out.push_str(&format!("\nOverall:\n{summary}\n"));
    }
    out.trim_end().to_string()
}
```

Run: `mise x -- cargo test -p balerix-plugin-web --lib review`
Expected: PASS.

- [ ] **Step 3: `Harness::post_route`**

In `crates/balerix-plugin-sdk/src/testing.rs`, after `get_route`:

```rust
    /// `POST /v1/routes<path>` with a JSON body as the daemon's proxy would
    /// send it: the bearer and `X-Balerix-Forwarded-Prefix: <prefix>`.
    pub async fn post_route(&self, path: &str, prefix: &str, body: &Value) -> (u16, Vec<u8>) {
        let resp = self
            .http
            .post(self.url(&format!("/v1/routes{path}")))
            .bearer_auth(&self.token)
            .header("x-balerix-forwarded-prefix", prefix)
            .json(body)
            .send()
            .await
            .unwrap_or_else(|e| panic!("Harness POST {path}: {e}"));
        let status = resp.status().as_u16();
        let body = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
        (status, body)
    }
```

- [ ] **Step 4: Write the failing plugin test**

In `crates/balerix-plugin-web/tests/plugin_it.rs` add (import `PluginAction` from `balerix_api`):

```rust
#[tokio::test]
async fn a_review_is_one_send_text_and_a_divider_in_the_column() {
    let (fake, _, h, _watch) = world().await;
    h.activate(ALICE, json!({})).await.unwrap();
    h.activate(BOB, json!({ "enabled": false })).await.unwrap();
    fake.set_workspace(ALICE, sample_diff(), BTreeMap::new());
    let review = json!({
        "head": sample_diff().head,
        "base_ref": "origin/main",
        "summary": "Looks fine.",
        "comments": [
            { "path": "src/lib.rs", "side": "new", "line": 2, "text": "+fn b() { c() }", "body": "Name this." }
        ]
    });
    let (status, body) = h.post_route("/agents/e2e/c/alice/review", "/v1/plugins/web", &review).await;
    assert_eq!((status, String::from_utf8_lossy(&body).as_ref()), (200, "{}"));
    let expected = "Review of e2e/c/alice against origin/main at 3f9c2a1 (1 comment)\n\nsrc/lib.rs line 2 (new):\n> +fn b() { c() }\nName this.\n\nOverall:\nLooks fine.";
    assert_eq!(
        fake.actions_for(ALICE),
        vec![PluginAction::SendText {
            text: expected.to_string(),
            submit: true
        }]
    );
    // the divider
    let (_, _, body) = h.get_route("/agents/e2e/c/alice/events.json", "/v1/plugins/web").await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    let last = v["events"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["name"], "review_sent");
    assert_eq!(last["summary"], "review sent (1 comment)");
    assert_eq!(last["payload"]["message"], expected);
    assert!(last["at"].as_u64().unwrap() > 1_700_000_000);
    // refusals: not enabled, a bad body, the caps, nothing to send
    let (status, _) = h.post_route("/agents/e2e/c/bob/review", "/v1/plugins/web", &review).await;
    assert_eq!(status, 404);
    let (status, body) = h
        .post_route("/agents/e2e/c/alice/review", "/v1/plugins/web", &json!({ "comments": [{ "path": "x" }] }))
        .await;
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&body));
    let (status, body) = h
        .post_route("/agents/e2e/c/alice/review", "/v1/plugins/web", &json!({}))
        .await;
    assert_eq!((status, String::from_utf8_lossy(&body).as_ref()), (400, "nothing to send"));
    let many: Vec<Value> = (0..201)
        .map(|i| json!({ "path": "a", "side": "new", "line": i, "text": "", "body": "b" }))
        .collect();
    let (status, body) = h
        .post_route("/agents/e2e/c/alice/review", "/v1/plugins/web", &json!({ "comments": many }))
        .await;
    assert_eq!((status, String::from_utf8_lossy(&body).as_ref()), (400, "comments: more than 200"));
    assert_eq!(fake.actions_for(ALICE).len(), 1, "no refused review was sent");
    // the daemon refusing the action: 502 with its message, nothing recorded
    fake.fail_actions(Some("f/c/a: tmux send-keys: no window"));
    let (status, body) = h.post_route("/agents/e2e/c/alice/review", "/v1/plugins/web", &review).await;
    assert_eq!(status, 502);
    assert!(String::from_utf8_lossy(&body).contains("no window"), "{}", String::from_utf8_lossy(&body));
    fake.fail_actions(None);
    let text = h.metrics().await;
    assert_eq!(metric(&text, "balerix_plugin_web_reviews_total", &[("outcome", "sent")]), Some(1.0));
    assert_eq!(metric(&text, "balerix_plugin_web_reviews_total", &[("outcome", "failed")]), Some(1.0));
    assert_eq!(metric(&text, "balerix_plugin_web_review_comments_total", &[]), Some(1.0));
    let (_, _, body) = h.get_route("/agents/e2e/c/alice/events.json?after=1", "/v1/plugins/web").await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert!(v["events"].as_array().unwrap().is_empty(), "a failed send adds no divider");
}
```

Run: `mise x -- cargo test -p balerix-plugin-web --test plugin_it a_review_is_one`
Expected: 404/405 from the router — the route does not exist.

- [ ] **Step 5: The route**

In `routes.rs` add `.route("/agents/{fleet}/{crew}/{agent}/review", get(review_page).post(post_review))` (replacing the `get(review_page)` line), the imports `axum::extract::rejection::JsonRejection`, `balerix_api::PluginAction`, `crate::review::{ReviewBody, render_message, validate}`, `crate::state::now`, and:

```rust
/// The submission (Spec C §4.4): validate, render one message, send it
/// as a `send_text` with submit, and append the `review_sent` divider.
/// A refused action is 502 with the daemon's text and leaves the draft
/// to the page.
async fn post_review(
    State(shared): State<Arc<Shared>>,
    Path(path): Path<(String, String, String)>,
    body: Result<Json<ReviewBody>, JsonRejection>,
) -> Response {
    let id = match enabled_id(&shared, path) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let Json(review) = match body {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, e.body_text()).into_response(),
    };
    if let Err(reason) = validate(&review) {
        return (StatusCode::BAD_REQUEST, reason).into_response();
    }
    let message = render_message(&id, &review);
    let n = review.comments.len();
    let action = PluginAction::SendText {
        text: message.clone(),
        submit: true,
    };
    match shared.host.action(&id, &action).await {
        Ok(()) => {
            shared.reviews_total.with_label_values(&["sent"]).inc();
            shared.review_comments_total.inc_by(n as u64);
            shared.cache.push_event(
                &id,
                now(),
                "review_sent",
                format!("review sent ({n} comment{})", if n == 1 { "" } else { "s" }),
                serde_json::json!({ "message": message, "comments": n }),
            );
            Json(serde_json::json!({})).into_response()
        }
        Err(e) => {
            shared.reviews_total.with_label_values(&["failed"]).inc();
            (StatusCode::BAD_GATEWAY, e.to_string()).into_response()
        }
    }
}
```

Run: `mise x -- cargo test -p balerix-plugin-web -p balerix-plugin-sdk` and clippy for the workspace.
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/balerix-plugin-web crates/balerix-plugin-sdk
git commit -q -F - > "$SCRATCH/commit.log" 2>&1 <<'EOF'
Send a review to the agent as one message

Spec C §4.4: `POST /agents/{id}/review` validates the page's body (200
comments, 64 KiB of text, the workspace path rule), renders the header,
the comments in file and line order each quoting its diff line, and the
summary, and sends it through `send_text` with submit. A sent review
appends the `review_sent` divider to the agent's activity buffer; a
refused action is a 502 that keeps the draft in the page.

Claude-Session: https://claude.ai/code/session_01XVWfZKBMR5cciaB4H5cXz5
EOF
grep -E "Summary|FAIL" "$SCRATCH/commit.log"; git log --oneline -1
```

---

### Task 9: The e2e, `verify-claude`, the docs, the threat model and Spec C §12

**Files:**
- Modify: `crates/balerix/tests/e2e.rs` (`web_journey`; a `raw_post` helper)
- Modify: `scripts/verify-claude.sh` (`browser_login`)
- Modify: `docs/THREAT-MODEL.md`, `ARCHITECTURE.md`, `AGENTS.md`, `README.md`
- Modify: `docs/superpowers/specs/2026-09-08-balerix-c-workspace-review-design.md` (§12)

**Interfaces:**
- Consumes: everything above through the real daemon: the mount, the cookie, `diff.json`, `POST review`, `events.json`, `fake-claude.stdin`.

- [ ] **Step 1: The e2e**

In `crates/balerix/tests/e2e.rs`, after `raw_get` add:

```rust
/// `POST` with explicit headers and a body, redirects not followed.
fn raw_post(
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, Vec<(String, String)>, String) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into();
    let mut req = agent.post(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut resp = req.send(body).unwrap();
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let text = resp.body_mut().read_to_string().unwrap();
    (status, headers, text)
}
```

In `web_journey`, after the terminal block (`rt.block_on(async { … });`) and before `// metrics: the proxy counted`, add:

```rust
    // the review page's data: alice's worktree through the workspace
    // routes, a review pasted into her stdin, the divider in her column
    let (status, _, page) = raw_get(
        &format!("{mount}agents/e2e/c/alice/review"),
        &[("Cookie", &cookie)],
    );
    assert_eq!(status, 200);
    assert!(page.contains(r#"const prefix = "/v1/plugins/web""#), "{page}");
    assert!(body_index_has_review_link(&mount, &cookie));
    fs::write(w.agent_dir("alice").join("workspace/NOTES.md"), "agent notes\n").unwrap();
    let (status, _, body) = raw_get(
        &format!("{mount}agents/e2e/c/alice/diff.json"),
        &[("Cookie", &cookie)],
    );
    assert_eq!(status, 200, "{body}");
    let diff: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(diff["base_ref"], "origin/main");
    assert_eq!(diff["head"].as_str().unwrap().len(), 40);
    let notes = diff["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "NOTES.md")
        .unwrap_or_else(|| panic!("NOTES.md in {diff}"));
    assert_eq!(notes["status"], "added");
    assert_eq!(notes["uncommitted"], true);
    assert!(notes["patch"].as_str().unwrap().contains("+agent notes"), "{notes}");
    let (status, _, text) = raw_get(
        &format!("{mount}agents/e2e/c/alice/file?path=NOTES.md"),
        &[("Cookie", &cookie)],
    );
    assert_eq!((status, text.as_str()), (200, "agent notes\n"));
    let (status, _, text) = raw_get(
        &format!("{mount}agents/e2e/c/alice/file?path=../home/.claude/settings.json"),
        &[("Cookie", &cookie)],
    );
    assert_eq!(status, 400, "{text}");
    assert!(text.contains("invalid path"), "{text}");
    // the column already holds fake-claude's startup events
    let (status, _, body) = raw_get(
        &format!("{mount}agents/e2e/c/alice/events.json"),
        &[("Cookie", &cookie)],
    );
    assert_eq!(status, 200, "{body}");
    let events: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "PreToolUse"),
        "{events}"
    );
    let review = serde_json::json!({
        "head": diff["head"],
        "base_ref": diff["base_ref"],
        "summary": "Looks fine.",
        "comments": [{ "path": "NOTES.md", "side": "new", "line": 1, "text": "+agent notes",
                       "body": "Please expand these notes." }]
    });
    let (status, _, body) = raw_post(
        &format!("{mount}agents/e2e/c/alice/review"),
        &[
            ("Cookie", &cookie),
            ("Sec-Fetch-Site", "same-origin"),
            ("Content-Type", "application/json"),
        ],
        review.to_string().as_bytes(),
    );
    assert_eq!((status, body.as_str()), (200, "{}"));
    let stdin = wait_file_until(
        &w.agent_dir("alice").join("home/fake-claude.stdin"),
        |s| s.contains("Please expand these notes."),
    );
    assert!(stdin.contains("Review of e2e/c/alice against origin/main at "), "{stdin}");
    assert!(stdin.contains("NOTES.md line 1 (new):\n> +agent notes\nPlease expand these notes."), "{stdin}");
    assert!(stdin.contains("Overall:\nLooks fine."), "{stdin}");
    let start = Instant::now();
    loop {
        let (_, _, body) = raw_get(
            &format!("{mount}agents/e2e/c/alice/events.json"),
            &[("Cookie", &cookie)],
        );
        if body.contains("\"review_sent\"") && body.contains("review sent (1 comment)") {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "no divider: {body}");
        std::thread::sleep(Duration::from_millis(200));
    }
    // a cross-origin POST is refused by the daemon, not the plugin
    let (status, _, _) = raw_post(
        &format!("{mount}agents/e2e/c/alice/review"),
        &[
            ("Cookie", &cookie),
            ("Origin", "http://evil.example"),
            ("Content-Type", "application/json"),
        ],
        review.to_string().as_bytes(),
    );
    assert_eq!(status, 403);
```

with the small helper beside `raw_post`:

```rust
fn body_index_has_review_link(mount: &str, cookie: &str) -> bool {
    let (_, _, body) = raw_get(mount, &[("Cookie", cookie)]);
    body.contains("/v1/plugins/web/agents/e2e/c/alice/review\">review</a>")
}
```

In the metrics wait later in the journey, add `&& m.contains("balerix_plugin_web_reviews_total{outcome=\"sent\"} 1")` to the condition. In the "no secret leaks" block add:

```rust
    // no workspace read went near the agent's home
    assert!(
        !log.contains(&w.agent_dir("alice").join("home").display().to_string()),
        "server.log names the agent's home"
    );
```

Run: `mise run e2e` (or `mise x -- cargo nextest run -p balerix --test e2e web_journey`).
Expected: PASS. If `fake-claude.stdin` shows the review's lines but not in one block, the paste reached `fake-claude` line by line, which is what its pump records; the assertions above are on substrings and hold either way.

- [ ] **Step 2: `verify-claude`**

In `scripts/verify-claude.sh`, `browser_login()`, after the `>>>     $LOGIN` line add:

```sh
    say ">>> Then click 'review' beside $AGENT: comment on a line, add a summary, Send review."
    say ">>> The verdict (Spec C §8): the agent's terminal shows ONE pasted message, and the"
    say ">>> activity column shows 'review sent' followed by the agent's tool calls."
```

Run `BALERIX_VERIFY_FAKE=1 mise run verify-claude` to see the lines print; run the real `mise run verify-claude` once, follow them, and record the verdict in Spec C §8 (step 5).

- [ ] **Step 3: The threat model and the architecture map**

`docs/THREAT-MODEL.md`:

- In the *Plugin ↔ daemon* boundary line, the host route list becomes "(`fleets`, `actions`, `kv`, `attach`, `fleets/watch`, `agents/*/workspace/*`)".
- Under **Out of scope / accepted risks**, after the `attach` bullet:

```
- **A plugin with `workspace` reads every worktree it is active for** — the operator's choice in `needs`, as for `attach`. It never reaches `home/` beside the worktree, the crew repo, or another agent's worktree: the path rule refuses `..`, absolute paths and `.git`, a symlink is never followed, and the canonical path is checked against the canonical worktree (`crates/balerix-api/src/workspace.rs::check_path`, `crates/balerix-runtime/src/inspect.rs`).
- **The daemon runs read-only `git` in a repository an agent can write to.** `worktree add` already runs there and applies the repository's smudge filters. `diff` can still run clean filters declared through `.gitattributes` plus `.git/config`; `-c core.fsmonitor=false`, `-c core.hooksPath=<empty>`, `--no-ext-diff`, `--no-textconv` and `GIT_OPTIONAL_LOCKS=0` close fsmonitor, hooks, external diff and textconv, and the residue is the existing "agents in a crew share `.git`" trust.
```

- In the **Mitigations** table, after the "Terminal bytes or fleet data" row:

```
| A workspace read escaping the worktree, or touching the agent's home | `check_path` before any I/O (no `..`, no absolute, no `.git` segment, no `\`/NUL, 4096 bytes); `symlink_metadata` must be a regular file or directory; canonical-prefix check; 1 MiB per file, 256 KiB per patch, 500 files; routes need the plugin's bearer, `workspace` in `needs` and an active pair | `balerix-api/src/workspace.rs`, `balerix-runtime/src/inspect.rs`, `plugin_api.rs::workspace_caller` |
| Repository config running a program under the daemon | every workspace git call: `GIT_*` scrubbed, `GIT_OPTIONAL_LOCKS=0`, `-c core.fsmonitor=false`, `-c core.hooksPath=<empty dir>`, `--no-ext-diff --no-textconv --no-color`; never `fetch`, never a credential helper; `inspect_it` plants an fsmonitor script and asserts it never ran | `balerix-runtime/src/inspect.rs` |
| Script injection through diff content, event payloads or review text | the review page renders every value with `textContent`; the id and prefix reach its script as JSON literals; comment paths pass `check_path`; bodies are capped (200 comments, 64 KiB) | `balerix-plugin-web/src/routes.rs::review_html`, `src/review.rs::validate` |
```

`ARCHITECTURE.md`:

- In "The pieces", `balerix-core`'s ports list becomes "`Materializer`, `AgentRunner`, `Clock`, `WorkspaceReader` today; `FleetStore` and `EventHandler` arrive with the server", and `balerix-runtime` gains "`inspect.rs` reads a worktree for the plugin host's workspace routes". `balerix-plugin-web`'s entry gains ", the review page with the diff, line comments and the agent's hook events (`review.rs`, `state.rs`)".
- In "How it flows", after the phase 3 paragraph:

```
**Workspace reads and review (Spec C):** a plugin declaring `workspace`
reads an agent's worktree through the daemon — `GET
/v1/plugin-host/agents/{id}/workspace/{diff,file,tree}`, gated like
`attach` by the capability and an active pair — and the daemon answers
through the `WorkspaceReader` port, which the runtime implements over
`git` in the worktree with the repository's config escape hatches
closed. The diff is the worktree against the merge-base with
`origin/<crew ref>`, committed, uncommitted and untracked alike. `web`
observes every hook event into a per-agent buffer and serves a review
page: the diff with line comments, a collapsible activity column, and a
submit that renders one message and sends it through `send_text`, which
tmux delivers as a bracketed paste when the text has newlines.
```

- Under "Non-obvious decisions", two bullets:

```
- **Git is run by the daemon, never granted to a plugin.** A read-only
  nono grant on `workspace/` would be fixed at plugin start, would need
  the crew repo too (a worktree's `.git` is a file pointing there) and
  would sit beside `home/`; the typed routes keep one audited set of git
  invocations, in a repository an agent can write to (Spec C PC-1, PC-2).
- **A review is a paste.** The review reaches the agent as one
  `send_text`; `TmuxRunner` sends multi-line text through a named buffer
  and `paste-buffer -p`, so Claude Code takes it as one message; a
  literal newline through `send-keys -l` is Ctrl-J to the application
  (Spec C PC-5).
```

`AGENTS.md` gotchas (append):

```
- `send_text` with a newline goes through `set-buffer` + `paste-buffer -p -d`
  (a bracketed paste), single lines through `send-keys -l`. The e2e's
  `fake-claude` records a paste line by line; the real `claude` takes it as
  one message (`verify-claude`).
- Workspace git calls (`balerix-runtime/src/inspect.rs`) set
  `GIT_OPTIONAL_LOCKS=0` and `-c core.fsmonitor=false -c core.hooksPath=<empty>`
  and pass `--no-ext-diff --no-textconv` to every `diff`: the crew's
  `.git/config` is agent-writable. Keep those when adding a git call there.
- A workspace route's two 404s differ on purpose: `no workspace for agent`
  (no worktree yet) and `no such path`; the SDK maps only the second to
  `None`.
- `web`'s event buffer is memory only: after a plugin restart the review
  page's column is empty until new events arrive (no observer catch-up).
```

`README.md`: the **Status** paragraph becomes "Spec A, Spec B (plugins) and Spec C (workspace reads and browser code review) are complete: …, and the web plugin's review page." Add after the phase 3 upgrade section:

```
### Upgrading to Spec C
- `balerix-plugin.yaml` may declare `needs: [workspace]` for the three
  read-only worktree routes (plugin-protocol §3 "Workspace"). The web
  plugin's manifest now declares `actions` and `workspace` and observes
  every hook event; re-run `mise run package-plugins`.
- `balerix-plugin-sdk`: `Host::{workspace_diff, workspace_file,
  workspace_tree}`, `FakeHost::{set_workspace, fail_actions}`,
  `Harness::post_route` are new; nothing existing changed.
- `send_text` with a newline is now a bracketed paste on tmux.
```

- [ ] **Step 4: Spec C §12**

Append to `docs/superpowers/specs/2026-09-08-balerix-c-workspace-review-design.md`:

```
## 12. Refinements from the plan (2026-09-08)

- **`check_path` lives in `balerix-api::workspace`**, not `balerix-core`
  (§3.1 said core): the web plugin validates comment paths with the same
  rule and may depend on `balerix-api` only; the function is pure, like
  `ResizeFrame::parse` already there.
- **The submission body carries `base_ref`** beside `head` (§4.4), so the
  message header names the base without a second diff call.
- **`Cmd::run_with_exit_codes`**: `git diff --no-index` exits 1 when the
  files differ; the runtime treats that as the answer.
- **A renamed file's per-file diff names both paths** (`-- <old> <new>`);
  with the new path alone git reports an addition.
- **`old_path` is always serialized**, `null` when absent, as §2.2's
  example shows.
- **`FakeHost::fail_actions`** stands in for a runner failure so the web
  plugin's 502 path is tested through the harness.
- **`Ports.workspace` landed with the daemon routes** (build order item 3,
  not 1), since the binary wires it to the runtime implementation.
- **§8 verdicts**: recorded at implementation — the bracketed paste
  against the real `claude` (`verify-claude`), `diff --no-index` inside a
  worktree, and the command-line `core.fsmonitor=false` override
  (`inspect_it`).
```

Fill in the three §8 verdicts in the table with the date and the test or run that proved each ("Verified 2026-09-XX (…)"), as the plugins spec's §11.1 does.

- [ ] **Step 5: Run everything and commit**

Run: `mise run check`, `mise run test-it`, `mise run e2e`, and `mise run verify-claude` once by hand for the §8 paste verdict.
Expected: all green; the by-hand run shows one pasted message in the agent's terminal and the `review sent` divider in the column.

```bash
git add crates/balerix/tests/e2e.rs scripts/verify-claude.sh docs ARCHITECTURE.md AGENTS.md README.md
git commit -q -F - > "$SCRATCH/commit.log" 2>&1 <<'EOF'
Close Spec C: the e2e review journey, the threat model and the docs

The web journey now writes into alice's worktree, reads it back through
diff.json and file, posts a review through the mount and finds it in
fake-claude's stdin with the divider in her activity column. The threat
model records the workspace reads and the daemon running git in an
agent-writable repository; ARCHITECTURE, AGENTS and README describe the
port, the routes, the paste and the upgrade; Spec C §12 records the
plan's refinements and §8 the verdicts.

Claude-Session: https://claude.ai/code/session_01XVWfZKBMR5cciaB4H5cXz5
EOF
grep -E "Summary|FAIL" "$SCRATCH/commit.log"; git log --oneline -1
```

Then open the pull request from `spec-c-workspace-review` with the Spec C summary and the session link `https://claude.ai/code/session_01XVWfZKBMR5cciaB4H5cXz5` at the end of its description.
