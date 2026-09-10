# Balerix Spec D — The Live Diff Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A fourth workspace route, `version`, answering a cheap fingerprint of the agent's worktree, and a review page that polls it through `events.json` and re-renders the diff when it changes — automatically, rate limited, deferred while a comment box is open, comments following moved lines, scroll kept, with an "updated N s ago" indicator.

**Architecture:** Seven tasks, each mergeable, in Spec D §8's order. `WorkspaceVersion` and `WorkspaceReader::version` land first with the pure `fingerprint_of` and the fake; the runtime computes it with three name-only git calls through the existing hardened `inspect_git`; the daemon route, the SDK method, the fake host and the fixture follow; then the web plugin folds the version into `events.json`, and the page grows the apply logic. The e2e and the docs close it.

**Tech Stack:** Rust 1.98.1 (edition 2024); axum 0.8.9, reqwest 0.13.4, serde, sha2 0.11.0, hex 0.4.3, tracing 0.1.44 (all already in `[workspace.dependencies]`; `balerix-runtime` and `balerix-plugin-sdk` gain `sha2`/`hex`, `balerix-plugin-web` gains `tracing` as ordinary dependencies — **no new workspace dependency**); real git 2.47 and tmux 3.7c for the integration and e2e tests. No new vendored JavaScript.

**Spec:** `docs/superpowers/specs/2026-09-08-balerix-d-live-diff-design.md` (Spec D), on top of Spec C (`docs/superpowers/specs/2026-09-08-balerix-c-workspace-review-design.md`) §2–§4 and `docs/plugin-protocol.md`. Read Spec D whole before any task. Where this plan refines Spec D (recorded again in Task 7 as Spec D §11):

- **`diff_refreshes_total` counts `diff.json` responses served**, not browser-side applies (the plugin cannot see an apply; every auto-refresh is exactly one fetch).
- **The fake fingerprints** (core's `FakeWorkspace` and the SDK's `FakeHost`) are `sha256(head \0 (path \0 bytes \0)*)` over the file map, so the fixture's value is derivable with `sha256sum`.
- **The page's first diff comes from the first poll** (which carries the fingerprint), not from a separate `loadDiff()` on load; `loadDiff()` remains the manual "Reload diff" path and the way a daemon refusal reaches the banner.
- **`anchorComments` is checked with `node` by the implementer, not by a Rust test**: `node` is not a `mise.toml` tool, so a test could not require it.

## Global Constraints

Copied from the specs and the earlier plans; every task's requirements include these.

- Rust **1.98.1**, `edition = "2024"`. `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` pass at every commit. `unsafe_code = "forbid"`. No `unwrap`/`expect` outside tests (test modules and `tests/*.rs` carry `#![allow(clippy::unwrap_used, clippy::expect_used)]`). Run cargo as `mise x -- cargo …`.
- **No new workspace dependencies.** The `Cargo.toml` changes are: `sha2`, `hex` under `balerix-runtime` and `balerix-plugin-sdk` `[dependencies]`; `tracing` under `balerix-plugin-web` `[dependencies]` — each as `{ workspace = true }`.
- Dependency direction: `api` leaf → `core` → `runtime` / `server` / `plugin-sdk` → binary; plugin crates depend on `balerix-plugin-sdk` and `balerix-api` only. `balerix-server` never imports `balerix-runtime`.
- Every git call in `crates/balerix-runtime/src/inspect.rs` goes through `inspect_git` (the five `GIT_*` variables scrubbed, `GIT_OPTIONAL_LOCKS=0`, `GIT_CEILING_DIRECTORIES`, `-c core.fsmonitor=false`, `-c core.hooksPath=<empty dir>`, and `DIFF_FLAGS` on every `diff`); the `FILTER_KEYS` refusal runs before any other git call; never `fetch`.
- The fingerprint is `hex(sha256(head \0 merge_base \0 (path \0 size \0 mtime.nsec \0 | path \0 missing \0)*))` over the sorted, deduplicated union of `diff --name-only -z <merge_base>`, `diff --name-only -z HEAD` and `ls-files --others --exclude-standard -z`, each path's `symlink_metadata` never following a symlink (Spec D §2.3).
- `events.json` becomes `{ phase, events, workspace }` with `workspace` `{ head, fingerprint }` or `null`; the page applies a changed diff automatically, at most once per **3 s**, deferred while a comment box is open; re-anchoring by `(path, side, text)` when exactly one line matches; scroll kept; indicator `updated N s ago` ticking every second (Spec D §3).
- Every value the pages render goes through `html_escape` (server) or `textContent` (browser).
- Integration and e2e tests skip with a printed reason when a tool is missing; run them with `BALERIX_REQUIRE_TOOLS=1`. Temp roots under `target/tmp`.
- Commit messages: imperative subject, body explains why, trailer `Claude-Session: https://claude.ai/code/session_01QG3jF1gRcquRpDr3UFQDfP`. Commit with the output redirected to a file (the hook runs the full check and prints thousands of lines). Branch: `spec-d-live-diff`, holding the Spec D commit.

---

## File structure

```
crates/balerix-api/src/workspace.rs         + WorkspaceVersion
crates/balerix-api/src/lib.rs               re-export
crates/balerix-core/src/ports.rs            + WorkspaceReader::version
crates/balerix-core/src/fakes.rs            + FakeWorkspace::version, fake_fingerprint
crates/balerix-runtime/Cargo.toml           + sha2, hex
crates/balerix-runtime/src/inspect.rs       + PathStat, fingerprint_of, refuse_filters (extracted), Runtime::version
crates/balerix-runtime/tests/inspect_it.rs  + the version cases
crates/balerix-server/src/plugin_api.rs     + the route and handler
crates/balerix-server/tests/workspace_it.rs + version assertions
crates/balerix-plugin-sdk/Cargo.toml        + sha2, hex
crates/balerix-plugin-sdk/src/host.rs       + workspace_version
crates/balerix-plugin-sdk/src/testing.rs    + the route, fake_fingerprint
crates/balerix-plugin-sdk/tests/conformance.rs  22; the replay
docs/plugin-protocol/workspace-version.json new
docs/plugin-protocol.md                     row, paragraph sentence, count
crates/balerix-plugin-web/Cargo.toml        + tracing
crates/balerix-plugin-web/src/state.rs      Events.workspace
crates/balerix-plugin-web/src/plugin.rs     + diff_refreshes_total, version_failures_total
crates/balerix-plugin-web/src/routes.rs     events_json calls workspace_version; diff_json counts; the page
crates/balerix-plugin-web/tests/plugin_it.rs  events.json workspace field, metrics
crates/balerix/tests/e2e.rs                 web_journey: the fingerprint changes
scripts/verify-claude.sh                    one line
docs/THREAT-MODEL.md, ARCHITECTURE.md, AGENTS.md, README.md, Spec D §7 and §11
```

---

### Task 1: `WorkspaceVersion`, the port method, `fingerprint_of`, the fake

**Files:**
- Modify: `crates/balerix-api/src/workspace.rs` (after `WorkspaceTree`), `crates/balerix-api/src/lib.rs` (the `workspace` re-export)
- Modify: `crates/balerix-core/src/ports.rs` (`WorkspaceReader`, its `balerix_api` import), `crates/balerix-core/src/fakes.rs`
- Modify: `crates/balerix-runtime/Cargo.toml`, `crates/balerix-runtime/src/inspect.rs` (pure functions only in this task)

**Interfaces:**
- Produces:
  ```rust
  // balerix_api
  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
  pub struct WorkspaceVersion { pub head: String, pub fingerprint: String }
  // balerix_core::WorkspaceReader
  fn version(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceVersion, WorkspaceError>;
  // balerix_core::fakes (pub, reused by nothing else in core; the SDK duplicates it — it may not depend on core)
  pub fn fake_fingerprint(head: &str, files: &BTreeMap<String, Vec<u8>>) -> String;
  // balerix_runtime::inspect
  pub struct PathStat { pub path: String, pub stat: Option<(u64, i64, i64)> }   // (size, mtime, mtime_nsec); None = missing
  pub fn fingerprint_of(head: &str, merge_base: &str, entries: &[PathStat]) -> String;
  ```

- [ ] **Step 1: The wire type and its test**

In `crates/balerix-api/src/workspace.rs`, after `WorkspaceTree`:

```rust
/// `GET …/workspace/version` (Spec D §2.1): `head` is the full sha;
/// `fingerprint` is 64 hex chars over `HEAD`, the merge-base and the size
/// and mtime of every changed or untracked path. Compared for equality,
/// never interpreted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceVersion {
    pub head: String,
    pub fingerprint: String,
}
```

Add `WorkspaceVersion` to the `pub use workspace::{…}` list in `lib.rs` (alphabetical: after `WorkspaceTree`). In `workspace.rs`'s test module, append to `the_diff_and_tree_round_trip_with_lowercase_enums`:

```rust
        let v = WorkspaceVersion {
            head: "h".repeat(40),
            fingerprint: "f".repeat(64),
        };
        let j = serde_json::to_value(&v).unwrap();
        assert_eq!(j, json!({ "head": "h".repeat(40), "fingerprint": "f".repeat(64) }));
        assert_eq!(serde_json::from_value::<WorkspaceVersion>(j).unwrap(), v);
```

Run: `mise x -- cargo test -p balerix-api workspace`
Expected: PASS.

- [ ] **Step 2: Write the failing core test**

In `crates/balerix-core/src/fakes.rs`'s `mod tests`, append to `the_fake_workspace_answers_from_what_the_test_set` (before its closing brace; `w` holds `f/c/a` with `src/lib.rs`, `src/sub/x.rs`, `README`, and `f/c/b` with `big`):

```rust
        let v = w.version(&id("f/c/a"), "origin/main").unwrap();
        assert_eq!(v.head, "h");
        assert_eq!(v.fingerprint.len(), 64);
        assert!(v.fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            w.version(&id("f/c/a"), "origin/main").unwrap().fingerprint,
            v.fingerprint,
            "stable"
        );
        w.set(
            &id("f/c/a"),
            diff.clone(),
            BTreeMap::from([("src/lib.rs".to_string(), b"fn a() {}\nfn b() {}\n".to_vec())]),
        );
        assert_ne!(
            w.version(&id("f/c/a"), "origin/main").unwrap().fingerprint,
            v.fingerprint,
            "different bytes, different fingerprint"
        );
        assert_eq!(
            w.version(&id("f/c/z"), "origin/main"),
            Err(WorkspaceError::Missing("f/c/z".into()))
        );
        assert!(w.calls().contains(&"version f/c/a origin/main".to_string()));
```

Run: `mise x -- cargo test -p balerix-core fakes`
Expected: compile error (no `version`).

- [ ] **Step 3: The port method and the fake**

In `crates/balerix-core/src/ports.rs`: add `WorkspaceVersion` to the `balerix_api` import and, after `list_dir` in `WorkspaceReader`:

```rust
    /// A cheap fingerprint of the worktree (Spec D §2): `HEAD`, the
    /// merge-base with `base_ref`, and every changed or untracked path's
    /// size and mtime. Equal fingerprints mean `diff` would answer the same.
    fn version(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceVersion, WorkspaceError>;
```

In `crates/balerix-core/src/fakes.rs`: add `WorkspaceVersion` to the `balerix_api` import; `use sha2::{Digest, Sha256};` (core already depends on `sha2` and `hex`). Before `impl WorkspaceReader for FakeWorkspace`:

```rust
/// The fakes' fingerprint: `sha256(head \0 (path \0 bytes \0)*)` over the
/// file map, so a `set` with different bytes changes it. The SDK's
/// `FakeHost` derives the same value (it may not depend on this crate),
/// and the `workspace-version.json` fixture holds it for its files.
pub fn fake_fingerprint(head: &str, files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut h = Sha256::new();
    h.update(head.as_bytes());
    h.update([0]);
    for (path, bytes) in files {
        h.update(path.as_bytes());
        h.update([0]);
        h.update(bytes);
        h.update([0]);
    }
    hex::encode(h.finalize())
}
```

and in the impl, after `list_dir`:

```rust
    fn version(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceVersion, WorkspaceError> {
        let _ = lock(&self.rec).record("version", &format!("{agent} {base_ref}"));
        self.with(agent, |t| {
            Ok(WorkspaceVersion {
                head: t.diff.head.clone(),
                fingerprint: fake_fingerprint(&t.diff.head, &t.files),
            })
        })
    }
```

`crates/balerix-core/src/lib.rs` declares `pub mod fakes;`, so `balerix_core::fakes::fake_fingerprint` is reachable with no re-export to add.

Run: `mise x -- cargo test -p balerix-core fakes` and `mise x -- cargo build --workspace --all-targets`
Expected: the core test PASSES; the build FAILS in `balerix-runtime` (`Runtime` does not implement `version`) — that is Task 2. To keep the workspace building at this commit, add to `crates/balerix-runtime/src/inspect.rs`'s `impl WorkspaceReader for Runtime` a temporary body that the next task replaces:

```rust
    fn version(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceVersion, WorkspaceError> {
        // Task 2 computes this; until then the diff's identity stands in.
        let d = self.diff(agent, base_ref)?;
        Ok(WorkspaceVersion {
            fingerprint: fingerprint_of(&d.head, &d.merge_base, &[]),
            head: d.head,
        })
    }
```

(adding `WorkspaceVersion` to the `balerix_api` import).

- [ ] **Step 4: Write the failing `fingerprint_of` test**

`crates/balerix-runtime/Cargo.toml` `[dependencies]`: add `sha2 = { workspace = true }` and `hex = { workspace = true }` (alphabetically). In `inspect.rs`'s `mod tests` add:

```rust
    #[test]
    fn the_fingerprint_changes_with_every_input_and_is_stable() {
        let a = PathStat { path: "a".into(), stat: Some((1, 10, 500)) };
        let b = PathStat { path: "b".into(), stat: None };
        let base = fingerprint_of("h", "m", &[a.clone(), b.clone()]);
        assert_eq!(base.len(), 64);
        assert_eq!(base, fingerprint_of("h", "m", &[a.clone(), b.clone()]), "stable");
        assert_ne!(base, fingerprint_of("H", "m", &[a.clone(), b.clone()]), "head");
        assert_ne!(base, fingerprint_of("h", "M", &[a.clone(), b.clone()]), "merge-base");
        assert_ne!(base, fingerprint_of("h", "m", &[a.clone()]), "a path");
        let bigger = PathStat { path: "a".into(), stat: Some((2, 10, 500)) };
        assert_ne!(base, fingerprint_of("h", "m", &[bigger, b.clone()]), "size");
        let later = PathStat { path: "a".into(), stat: Some((1, 11, 500)) };
        assert_ne!(base, fingerprint_of("h", "m", &[later, b.clone()]), "mtime");
        let nsec = PathStat { path: "a".into(), stat: Some((1, 10, 501)) };
        assert_ne!(base, fingerprint_of("h", "m", &[nsec, b.clone()]), "mtime nsec");
        let present = PathStat { path: "b".into(), stat: Some((0, 0, 0)) };
        assert_ne!(base, fingerprint_of("h", "m", &[a, present]), "missing vs present");
        assert_eq!(fingerprint_of("", "", &[]).len(), 64);
    }
```

Run: `mise x -- cargo test -p balerix-runtime --lib inspect`
Expected: compile error.

- [ ] **Step 5: `PathStat` and `fingerprint_of`**

In `inspect.rs`, after `shape_patch`:

```rust
/// One changed or untracked path as the fingerprint sees it: `(size,
/// mtime, mtime_nsec)` from `symlink_metadata`, or `None` when the path
/// vanished between the listing and the `stat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStat {
    pub path: String,
    pub stat: Option<(u64, i64, i64)>,
}

/// `hex(sha256(head \0 merge_base \0 (path \0 size \0 mtime.nsec \0 |
/// path \0 missing \0)*))` (Spec D §2.3). Entries are hashed in the order
/// given; callers pass them sorted.
pub fn fingerprint_of(head: &str, merge_base: &str, entries: &[PathStat]) -> String {
    let mut h = Sha256::new();
    h.update(head.as_bytes());
    h.update([0]);
    h.update(merge_base.as_bytes());
    h.update([0]);
    for e in entries {
        h.update(e.path.as_bytes());
        h.update([0]);
        match e.stat {
            Some((size, mtime, nsec)) => {
                h.update(size.to_string().as_bytes());
                h.update([0]);
                h.update(format!("{mtime}.{nsec:09}").as_bytes());
            }
            None => h.update(b"missing"),
        }
        h.update([0]);
    }
    hex::encode(h.finalize())
}
```

with `use sha2::{Digest, Sha256};` in the imports.

Run: `mise x -- cargo test -p balerix-runtime --lib inspect`, `mise x -- cargo test -p balerix-core -p balerix-api`, `mise x -- cargo clippy --workspace --all-targets -- -D warnings`, `mise x -- cargo fmt --all --check`
Expected: PASS, clean (the workspace builds: server and SDK tests use `FakeWorkspace`/`Runtime`, both of which now implement `version`).

- [ ] **Step 6: Commit**

Subject: `Add WorkspaceVersion, the version port method and the fingerprint`. Body: Spec D §2.1/§2.3, the pure function, the fake's derivation shared with the SDK's fake by value, the temporary runtime body replaced in the next commit. Trailer as in Global Constraints.

---

### Task 2: `Runtime::version` over real git

**Files:**
- Modify: `crates/balerix-runtime/src/inspect.rs` (`diff` — extract `refuse_filters`; the real `version`)
- Modify: `crates/balerix-runtime/tests/inspect_it.rs`

**Interfaces:**
- Consumes: `inspect_git`, `DIFF_FLAGS`, `FILTER_KEYS`, `split_z`, `PathStat`, `fingerprint_of`.
- Produces: `impl WorkspaceReader for Runtime { fn version … }`; a private `fn refuse_filters(git: &dyn Fn(&[&str], &[i32]) -> Result<String, WorkspaceError>) -> Result<(), WorkspaceError>` used by `diff` and `version`.

- [ ] **Step 1: Write the failing integration cases**

In `crates/balerix-runtime/tests/inspect_it.rs` (no new import is needed: `WorkspaceReader` is already in scope and `version` returns through it), add these blocks. First, right after the "no worktree yet" assertion at the top of the test (`assert_eq!(rt.diff(&id, "origin/main"), Err(WorkspaceError::Missing("f/c/a".into())))`):

```rust
    assert_eq!(
        rt.version(&id, "origin/main"),
        Err(WorkspaceError::Missing("f/c/a".into()))
    );
```

Then, immediately after the first `let d = rt.diff(&id, "origin/main").unwrap();` block's assertions (before `// file and tree`):

```rust
    // the version: stable while nothing changes, sensitive to every kind of change
    let v0 = rt.version(&id, "origin/main").unwrap();
    assert_eq!(v0.head, head);
    assert_eq!(v0.fingerprint.len(), 64);
    assert_eq!(rt.version(&id, "origin/main").unwrap(), v0, "stable");
    std::fs::write(w.join("src/lib.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
    let v1 = rt.version(&id, "origin/main").unwrap();
    assert_ne!(v1.fingerprint, v0.fingerprint, "an edit");
    std::fs::write(w.join("fresh.txt"), "new\n").unwrap();
    let v2 = rt.version(&id, "origin/main").unwrap();
    assert_ne!(v2.fingerprint, v1.fingerprint, "an untracked file");
    git(w, &["add", "notes.txt"]);
    let v3 = rt.version(&id, "origin/main").unwrap();
    assert_eq!(v3.fingerprint, v2.fingerprint, "a byte-identical git add is invisible");
    git(w, &["commit", "-q", "-m", "notes"]);
    let v4 = rt.version(&id, "origin/main").unwrap();
    assert_ne!(v4.fingerprint, v3.fingerprint, "a commit");
    assert_ne!(v4.head, v0.head);
    std::fs::remove_file(w.join("fresh.txt")).unwrap();
    let v5 = rt.version(&id, "origin/main").unwrap();
    assert_ne!(v5.fingerprint, v4.fingerprint, "a deletion");
    // put the tree back as the later assertions expect it
    git(w, &["reset", "-q", "--soft", "HEAD~1"]);
    git(w, &["reset", "-q", "notes.txt"]);
    std::fs::write(w.join("src/lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    assert_eq!(rt.diff(&id, "origin/main").unwrap().files.len(), d.files.len(), "restored");
```

(`head` is the `let head = git(w, &["rev-parse", "HEAD"])…` already in the test. The restore block: `reset --soft HEAD~1` un-commits `notes` leaving it staged; `reset notes.txt` unstages it back to untracked; the edit is reverted. `fresh.txt` is already gone.)

Then, in the filter case that sets `filter.pwn.clean` (the first one), after the existing refusal assertion on `rt.diff`, add:

```rust
    assert!(
        matches!(rt.version(&id, "origin/main"), Err(WorkspaceError::Filter { .. })),
        "version is refused by the same check"
    );
```

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo test -p balerix-runtime --test inspect_it`
Expected: FAIL — with the temporary body, `v3.fingerprint == v2.fingerprint` holds but "an untracked file" already fails (the stand-in ignores paths), or the edit case fails; record the first failing assertion.

- [ ] **Step 2: The implementation**

In `inspect.rs`, extract the filter check from `diff` into a free function placed after `confine`:

```rust
/// The `FILTER_KEYS` probe, first on every git-running read: `Filter` when
/// the repository config names a program git would run.
fn refuse_filters(
    git: &dyn Fn(&[&str], &[i32]) -> Result<String, WorkspaceError>,
) -> Result<(), WorkspaceError> {
    let filters = git(
        &[
            "config",
            "--local",
            "--includes",
            "--name-only",
            "--get-regexp",
            FILTER_KEYS,
        ],
        &[0, 1],
    )?;
    match filters.lines().next().map(str::trim).filter(|k| !k.is_empty()) {
        Some(key) => Err(WorkspaceError::Filter {
            key: key.to_string(),
        }),
        None => Ok(()),
    }
}
```

and in `diff`, replace the inline `let filters = … return Err(…)` block with `refuse_filters(&git)?;`. Replace Task 1's temporary `version` with:

```rust
    fn version(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceVersion, WorkspaceError> {
        let id = agent.to_string();
        let (paths, crew) = self.workspace_of(agent)?;
        let git = |args: &[&str], ok: &[i32]| self.inspect_git(&id, &crew, &paths, args, ok);
        refuse_filters(&git)?;
        let head = git(&["rev-parse", "HEAD"], &[0])?.trim().to_string();
        let merge_base = git(&["merge-base", base_ref, "HEAD"], &[0])?
            .trim()
            .to_string();
        let mut against_base: Vec<&str> = vec!["diff"];
        against_base.extend(DIFF_FLAGS);
        against_base.extend(["--name-only", "-z", merge_base.as_str()]);
        let mut against_head: Vec<&str> = vec!["diff"];
        against_head.extend(DIFF_FLAGS);
        against_head.extend(["--name-only", "-z", "HEAD"]);
        let mut paths_seen = split_z(&git(&against_base, &[0])?);
        paths_seen.extend(split_z(&git(&against_head, &[0])?));
        paths_seen.extend(split_z(&git(
            &["ls-files", "--others", "--exclude-standard", "-z"],
            &[0],
        )?));
        // `BTreeSet`: sorted and deduplicated, so the hash order is fixed.
        let entries: Vec<PathStat> = paths_seen
            .into_iter()
            .map(|path| {
                let stat = std::fs::symlink_metadata(paths.workspace.join(&path))
                    .ok()
                    .map(|m| (m.len(), m.mtime(), m.mtime_nsec()));
                PathStat { path, stat }
            })
            .collect();
        Ok(WorkspaceVersion {
            fingerprint: fingerprint_of(&head, &merge_base, &entries),
            head,
        })
    }
```

(`MetadataExt` is already imported. `split_z` returns a `BTreeSet<String>`; `extend` keeps it sorted and deduplicated.)

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo test -p balerix-runtime --test inspect_it`, `mise x -- cargo test -p balerix-runtime --lib inspect`, clippy for the crate, fmt.
Expected: PASS. If "stable" fails, a file's mtime moved between the two calls (nothing writes between them; investigate rather than loosen). If "an edit" fails, the host's mtime resolution is coarse (Spec D §7): fall back to hashing the first 4 KiB of each regular file into the entry — say so in the report.

- [ ] **Step 3: Commit**

Subject: `Compute the workspace version over real git`. Body: the three name-only calls, the stat, what a `git add` does not change and why (Spec D PD-5), the shared filter check. Trailer as in Global Constraints.

---

### Task 3: The daemon route

**Files:**
- Modify: `crates/balerix-server/src/plugin_api.rs` (`router()`, the `balerix_api` import, a handler after `workspace_tree`)
- Modify: `crates/balerix-server/tests/workspace_it.rs`

**Interfaces:**
- Consumes: `workspace_caller`, `Daemon::{base_ref, workspace}`, `WorkspaceReader::version`.
- Produces: `GET /v1/plugin-host/agents/{fleet}/{crew}/{agent}/workspace/version` → `WorkspaceVersion` (200); 403/404/500 as for `diff`.

- [ ] **Step 1: Write the failing server assertions**

In `crates/balerix-server/tests/workspace_it.rs`, the test binds `let web_tok = token(&w, "web").await;` and `let flow_tok = …` right after the `web.workspace_tree` assertions (`assert_eq!(tree.entries[1].name, "src");`). Immediately after those two bindings, add the raw assertions (the `Host` call arrives in Task 4):

```rust
    // the version: the diff's head and a 64-hex fingerprint over the files
    let (s, v) = w.api.plugin(
        &web_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/a/workspace/version",
        None,
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["head"], "h".repeat(40));
    assert_eq!(v["fingerprint"].as_str().unwrap().len(), 64);
    assert!(
        w.h.workspace
            .calls()
            .contains(&"version f/c/a origin/release".to_string())
    );
    let (s, v) = w.api.plugin(
        &web_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/b/workspace/version",
        None,
    );
    assert_eq!((s, v["error"].as_str()), (404, Some("no workspace for agent f/c/b")));
```

In the 403 block that calls `w.api.plugin(&flow_tok, "GET", ".../f/c/a/workspace/diff", None)`, add the same request against `/workspace/version` asserting 403 and the capability text.

Run: `mise x -- cargo test -p balerix-server --test workspace_it`
Expected: FAIL — 404 from axum for the unknown route.

- [ ] **Step 2: The route**

In `crates/balerix-server/src/plugin_api.rs`: add `WorkspaceVersion` to the `balerix_api` import; in `router()` after the `workspace/tree` route:

```rust
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/workspace/version",
            get(workspace_version),
        )
```

and after `workspace_tree`:

```rust
/// Spec D §2.2: the worktree's fingerprint against the crew's base, gated
/// like `diff`.
async fn workspace_version(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
) -> Result<Json<WorkspaceVersion>, ApiError> {
    let agent = workspace_caller(&state, &headers, path).await?;
    let base_ref = state
        .daemon
        .base_ref(&agent)
        .await
        .ok_or(DaemonError::NotFound)?;
    let ws = state.daemon.workspace();
    let id = agent.clone();
    let version = tokio::task::spawn_blocking(move || ws.version(&id, &base_ref))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(version))
}
```

Run: `mise x -- cargo test -p balerix-server`, clippy for the crate, fmt.
Expected: PASS.

- [ ] **Step 3: Commit**

Subject: `Serve the workspace version route from the daemon`. Trailer as in Global Constraints.

---

### Task 4: SDK — `Host::workspace_version`, `FakeHost`, the fixture, the protocol doc

**Files:**
- Modify: `crates/balerix-plugin-sdk/Cargo.toml` (`sha2`, `hex`), `src/host.rs` (after `workspace_tree`), `src/testing.rs` (the route, a handler, `fake_fingerprint`)
- Create: `docs/plugin-protocol/workspace-version.json`
- Modify: `crates/balerix-plugin-sdk/tests/conformance.rs` (count 22, the replay), `crates/balerix-server/tests/workspace_it.rs` (the `Host` call), `docs/plugin-protocol.md`

**Interfaces:**
- Produces:
  ```rust
  pub async fn workspace_version(&self, agent: &str) -> Result<WorkspaceVersion, SdkError>;   // Host
  ```

- [ ] **Step 1: The fixture**

The fixture's `fingerprint` is `fake_fingerprint("b7e0d44a1c2f3e4d5a6b7c8d9e0f1a2b3c4d5e6f", {"src/lib.rs": "fn main() {}\n"})`, computed as

```sh
printf 'b7e0d44a1c2f3e4d5a6b7c8d9e0f1a2b3c4d5e6f\0src/lib.rs\0fn main() {}\n\0' | sha256sum
```

Write `docs/plugin-protocol/workspace-version.json` with that value in place of `<FP>`:

```json
{
  "route": "GET /v1/plugin-host/agents/payments/backend/bob/workspace/version",
  "direction": "plugin-to-daemon",
  "request": null,
  "status": 200,
  "response": { "head": "b7e0d44a1c2f3e4d5a6b7c8d9e0f1a2b3c4d5e6f", "fingerprint": "<FP>" }
}
```

(`head` is `workspace-diff.json`'s `head`; the conformance replay sets the fake's workspace from that diff and the `workspace-file.json` bytes, which are exactly `src/lib.rs` → `fn main() {}\n`.)

- [ ] **Step 2: Write the failing conformance additions**

In `crates/balerix-plugin-sdk/tests/conformance.rs`: change the count assertion `out.len(), 21` to `22`; after the `workspace_tree` assertion in `the_host_sends_every_plugin_to_daemon_fixture_and_reads_the_answer` add:

```rust
    assert_eq!(
        serde_json::to_value(host.workspace_version("payments/backend/bob").await.unwrap())
            .unwrap(),
        fx["workspace-version"]["response"]
    );
    let e = host
        .workspace_version("payments/backend/nobody")
        .await
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "daemon: HTTP 404: no workspace for agent payments/backend/nobody"
    );
```

Run: `mise x -- cargo test -p balerix-plugin-sdk --test conformance`
Expected: compile error (no `workspace_version`).

- [ ] **Step 3: `Host::workspace_version`**

In `crates/balerix-plugin-sdk/src/host.rs`: add `WorkspaceVersion` to the `balerix_api` import; after `workspace_tree`:

```rust
    /// `GET agents/{id}/workspace/version` (Spec D §2.2): a cheap
    /// fingerprint of the worktree; equal values mean `workspace_diff`
    /// would answer the same. 404 `no workspace for agent …` before the
    /// worktree exists.
    pub async fn workspace_version(&self, agent: &str) -> Result<WorkspaceVersion, SdkError> {
        self.json(
            self.http
                .get(self.url(&format!("agents/{agent}/workspace/version"))),
        )
        .await
    }
```

- [ ] **Step 4: `FakeHost`**

`crates/balerix-plugin-sdk/Cargo.toml` `[dependencies]`: add `sha2 = { workspace = true }` and `hex = { workspace = true }`. In `src/testing.rs`: add `WorkspaceVersion` to the `balerix_api` import and `use sha2::{Digest, Sha256};`. In `router`, after the `workspace/tree` route:

```rust
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/workspace/version",
            get(workspace_version),
        )
```

After `workspace_tree`:

```rust
/// The same derivation as `balerix_core::fakes::fake_fingerprint` (this
/// crate may not depend on core): `sha256(head \0 (path \0 bytes \0)*)`.
/// `docs/plugin-protocol/workspace-version.json` holds it for its files.
fn fake_fingerprint(head: &str, files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut h = Sha256::new();
    h.update(head.as_bytes());
    h.update([0]);
    for (path, bytes) in files {
        h.update(path.as_bytes());
        h.update([0]);
        h.update(bytes);
        h.update([0]);
    }
    hex::encode(h.finalize())
}

async fn workspace_version(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<(String, String, String)>,
) -> Response {
    with_workspace(&inner, &headers, id, |diff, files| {
        Json(WorkspaceVersion {
            head: diff.head.clone(),
            fingerprint: fake_fingerprint(&diff.head, files),
        })
    })
}
```

Run: `mise x -- cargo test -p balerix-plugin-sdk`
Expected: PASS (the replay's fingerprint equals the fixture's — if not, recompute the fixture with the `printf` above and check the `\n` inside the bytes).

- [ ] **Step 5: Task 3's `Host` call and the protocol doc**

In `crates/balerix-server/tests/workspace_it.rs`, before the raw `version` block of Task 3:

```rust
    let ver = web.workspace_version("f/c/a").await.unwrap();
    assert_eq!(ver.head, "h".repeat(40));
    assert_eq!(ver.fingerprint.len(), 64);
```

`docs/plugin-protocol.md` §3: after the `workspace/tree` row add

```
| `GET agents/…/workspace/version` | `workspace` | — | `{ head, fingerprint }` | 200 | `workspace-version.json` |
```

and to the **Workspace** paragraph append: "`version` (Spec D) answers `{ head, fingerprint }`: `head` is the worktree's `HEAD`, `fingerprint` 64 hex chars over `HEAD`, the merge-base and the size and mtime of every changed or untracked path — equal values mean `diff` would answer the same; compare, never parse." In §6, "twenty-one fixtures" becomes "twenty-two fixtures".

Run: `mise x -- cargo test -p balerix-server --test workspace_it`, `mise x -- cargo clippy --workspace --all-targets -- -D warnings`, fmt.
Expected: PASS, clean.

- [ ] **Step 6: Commit**

Subject: `Add the workspace version route to the SDK, the fake host and the protocol`. Trailer as in Global Constraints.

---

### Task 5: web — `events.json` carries the version; the metrics

**Files:**
- Modify: `crates/balerix-plugin-web/Cargo.toml` (`tracing`), `src/state.rs` (`Events`), `src/plugin.rs` (`Shared`, `new`), `src/routes.rs` (`events_json`, `diff_json`)
- Modify: `crates/balerix-plugin-web/tests/plugin_it.rs`

**Interfaces:**
- Consumes: `Host::workspace_version`, `Cache::events_after`.
- Produces:
  ```rust
  pub struct Events { pub phase: AgentPhase, pub events: Vec<Entry>, #[serde(default)] pub workspace: Option<WorkspaceVersion> }
  // Shared gains: pub diff_refreshes_total: IntCounter, pub version_failures_total: IntCounter
  // events.json → { phase, events, workspace: { head, fingerprint } | null }
  ```

- [ ] **Step 1: Write the failing plugin test**

In `crates/balerix-plugin-web/tests/plugin_it.rs` add (after `the_review_page_and_its_data_routes_pass_the_workspace_through`):

```rust
#[tokio::test]
async fn events_json_carries_the_workspace_version_when_there_is_one() {
    let (fake, _, h, _watch) = world().await;
    h.activate(ALICE, json!({})).await.unwrap();
    h.activate("e2e/c/carol", json!({})).await.unwrap();
    fake.set_workspace(
        ALICE,
        sample_diff(),
        BTreeMap::from([("src/lib.rs".to_string(), b"fn a() {}\n".to_vec())]),
    );
    let (status, _, body) = h
        .get_route("/agents/e2e/c/alice/events.json", "/v1/plugins/web")
        .await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["workspace"]["head"], sample_diff().head);
    let fp = v["workspace"]["fingerprint"].as_str().unwrap().to_string();
    assert_eq!(fp.len(), 64);
    fake.set_workspace(
        ALICE,
        sample_diff(),
        BTreeMap::from([("src/lib.rs".to_string(), b"fn a() {}\nfn b() {}\n".to_vec())]),
    );
    let (_, _, body) = h
        .get_route("/agents/e2e/c/alice/events.json", "/v1/plugins/web")
        .await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_ne!(v["workspace"]["fingerprint"], fp, "different bytes, different fingerprint");
    // carol has no workspace at the fake: null, and the column still flows
    let (status, _, body) = h
        .get_route("/agents/e2e/c/carol/events.json", "/v1/plugins/web")
        .await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert!(v["workspace"].is_null(), "{v}");
    assert_eq!(v["phase"], "pending");
    // one diff.json fetch is one refresh
    let (status, _, _) = h
        .get_route("/agents/e2e/c/alice/diff.json", "/v1/plugins/web")
        .await;
    assert_eq!(status, 200);
    let text = h.metrics().await;
    assert_eq!(metric(&text, "balerix_plugin_web_diff_refreshes_total", &[]), Some(1.0));
    assert_eq!(metric(&text, "balerix_plugin_web_version_failures_total", &[]), Some(1.0));
}
```

Run: `mise x -- cargo test -p balerix-plugin-web --test plugin_it events_json_carries`
Expected: FAIL — `workspace` absent.

- [ ] **Step 2: `Events.workspace`, the metrics, the route**

`crates/balerix-plugin-web/Cargo.toml` `[dependencies]`: add `tracing = { workspace = true }`. In `src/state.rs`: add `WorkspaceVersion` to the `balerix_api` import and change `Events` to

```rust
/// The `events.json` body (Spec D §3.1 adds `workspace`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Events {
    pub phase: AgentPhase,
    pub events: Vec<Entry>,
    /// The worktree's version, `None` when the daemon refused or failed.
    #[serde(default)]
    pub workspace: Option<WorkspaceVersion>,
}
```

and in `events_after`'s constructor add `workspace: None,` (`events_after` is the only place in the crate that constructs `Events`).

In `src/plugin.rs`: `Shared` gains

```rust
    pub diff_refreshes_total: IntCounter,
    pub version_failures_total: IntCounter,
```

registered in `new` after `events_buffered_total`:

```rust
        let diff_refreshes_total = metrics.int_counter(
            "diff_refreshes_total",
            "diff.json fetches, one per refresh of a review page's diff",
        )?;
        let version_failures_total = metrics.int_counter(
            "version_failures_total",
            "workspace_version calls that failed during an events.json poll",
        )?;
```

and stored in the `Shared { .. }` literal.

In `src/routes.rs`, `events_json` becomes:

```rust
/// The activity column's poll: entries after `after`, the phase, and the
/// worktree's version (Spec D §3.1) — `null` when the daemon refuses or
/// fails, so the column keeps flowing.
async fn events_json(
    State(shared): State<Arc<Shared>>,
    Path(path): Path<(String, String, String)>,
    q: Result<Query<AfterQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let id = match enabled_id(&shared, path) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let after = match q {
        Ok(Query(q)) => q.after,
        Err(e) => return (StatusCode::BAD_REQUEST, e.body_text()).into_response(),
    };
    let mut events = shared.cache.events_after(&id, after);
    match shared.host.workspace_version(&id).await {
        Ok(v) => events.workspace = Some(v),
        Err(e) => {
            tracing::debug!(agent = %id, "workspace version: {e}");
            shared.version_failures_total.inc();
        }
    }
    Json(events).into_response()
}
```

In `diff_json`, on the `Ok(diff)` arm, `shared.diff_refreshes_total.inc();` before returning `Json(diff)`.

Run: `mise x -- cargo test -p balerix-plugin-web`, clippy for the crate, fmt.
Expected: PASS (the existing `observed_events_feed_the_activity_column_of_enabled_agents` still passes: alice has no workspace there, so `workspace` is `null` and the metric it asserts is unchanged).

- [ ] **Step 3: Commit**

Subject: `Carry the workspace version in the web plugin's events.json`. Body: Spec D PD-2, one poll for both columns, `null` on refusal, the two metrics and what `diff_refreshes_total` counts (Spec D §11). Trailer as in Global Constraints.

---

### Task 6: The page — auto-apply, deferral, re-anchoring, scroll, the indicator

**Files:**
- Modify: `crates/balerix-plugin-web/src/routes.rs` (`review_html`: CSS, the header, the script; the unit test)

**Interfaces:**
- Consumes: `events.json`'s `workspace`, `diff.json`.
- Produces (inline script): `anchorComments(diff, comments) -> stale[]` (mutates `comment.line` on a unique re-anchor), `applyDiff(d, fp)`, `maybeApply()`, `boxOpen()`, `renderKeepingScroll()`, `fmtAge(ms)`; state `rendered`, `latestFp`, `pendingDiff`, `lastApplied`, `renderedAt`; constant `APPLY_MIN_MS = 3000`; markup `<span id="age"></span>` after `#meta`; `.file` elements carry `data-path`.

- [ ] **Step 1: Write the failing unit test**

In `routes.rs`'s `mod tests`, add to `the_review_page_links_its_routes_through_the_prefix_and_escapes_the_id`, after the `balerix-review/` assertion:

```rust
        assert!(page.contains("function anchorComments("), "re-anchoring is a standalone function");
        assert!(page.contains(r#"id="age""#), "the indicator");
        assert!(page.contains("const APPLY_MIN_MS = 3000;"), "the apply rate limit");
        assert!(page.contains("r.workspace"), "the poll reads the version");
        assert!(!page.contains("loadDiff();\npollEvents();"), "the first diff comes from the first poll");
```

Run: `mise x -- cargo test -p balerix-plugin-web --lib routes`
Expected: FAIL on `anchorComments`.

- [ ] **Step 2: CSS and markup**

In `review_html`'s `<style>` add, after the `#banner` rule:

```
#diff{{position:relative}}
#age{{color:#57606a;padding:0 .3rem;border-radius:3px;transition:background 1.2s}}
#age.flash{{background:#fff3b0;transition:none}}
```

(`#diff` already has a rule; add `position:relative` to it rather than a second `#diff` rule.) In the header, change `<span id="meta"></span>` to `<span id="meta"></span> <span id="age"></span>`.

- [ ] **Step 3: The script**

Replace the review page's `<script>` body from `let diff = null;` through the end (`setInterval(pollEvents, {poll});`) with the following; everything before (`prefix`, `id`, `key`) stays. Every literal `{`/`}` is doubled as in the surrounding `format!`; the only placeholder below is `{poll}`.

```js
let diff = null;
let pending = null;
let rendered = null;
let latestFp = null;
let pendingDiff = null;
let lastApplied = 0;
let renderedAt = null;
const APPLY_MIN_MS = 3000;
let draft = {{ comments: [], summary: "", collapsed: false }};
try {{ const s = localStorage.getItem(key); if (s) draft = Object.assign(draft, JSON.parse(s)); }} catch (e) {{}}
const transient = (k, v) => (k === "editing" || k === "typing") ? undefined : v;
function save() {{ try {{ localStorage.setItem(key, JSON.stringify(draft, transient)); }} catch (e) {{}} }}
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

// Spec D PD-4: a comment keeps its exact anchor; otherwise the lines of the
// same path and side with the same text are collected — exactly one hit
// re-anchors the comment to that line number, none or several make it
// stale. Returns the stale comments; re-anchored ones are updated in place.
function anchorComments(diff, comments) {{
  const exact = new Set(); const byText = new Map();
  for (const f of diff.files) for (const h of parsePatch(f.patch)) for (const l of h.lines) {{
    if (l.kind === "meta") continue;
    exact.add(f.path + " " + l.side + " " + l.line + " " + l.text);
    const k = f.path + " " + l.side + " " + l.text;
    byText.set(k, (byText.get(k) || []).concat([l.line]));
  }}
  const stale = [];
  for (const c of comments) {{
    if (exact.has(anchorOf(c))) continue;
    const hits = byText.get(c.path + " " + c.side + " " + c.text) || [];
    if (hits.length === 1) c.line = hits[0]; else stale.push(c);
  }}
  return stale;
}}

function commentRow(c, editing) {{
  const tr = el("tr", "comment"); const td = el("td"); td.colSpan = 3;
  if (editing) {{
    const ta = el("textarea"); ta.value = c.typing !== undefined ? c.typing : (c.body || "");
    ta.oninput = () => {{ c.typing = ta.value; }};
    const ok = el("button", "", "Save comment"); const no = el("button", "", "Cancel");
    ok.onclick = () => {{ if (!ta.value.trim()) return; c.body = ta.value; delete c.editing; delete c.typing; if (!draft.comments.includes(c)) draft.comments.push(c); pending = null; save(); render(); maybeApply(); }};
    no.onclick = () => {{ delete c.editing; delete c.typing; pending = null; render(); maybeApply(); }};
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
function boxOpen() {{ return pending !== null || draft.comments.some((c) => c.editing); }}

function renderFile(f) {{
  const box = el("div", "file"); box.dataset.path = f.path;
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
  const stale = anchorComments(diff, draft.comments);
  save();
  if (stale.length) {{
    const box = el("div"); box.id = "stale"; box.appendChild(el("strong", "", "no longer in the diff (still sent):"));
    for (const c of stale) {{ const row = el("div", "", c.path + " line " + c.line + " (" + c.side + "): " + c.body + " "); const del = el("button", "", "Delete"); del.onclick = () => {{ draft.comments = draft.comments.filter((x) => x !== c); save(); render(); }}; row.appendChild(del); box.appendChild(row); }}
    root.appendChild(box);
  }}
  if (!diff.files.length) root.appendChild(el("p", "", "no changes against " + diff.base_ref));
  for (const f of diff.files) root.appendChild(renderFile(f));
  document.getElementById("count").textContent = draft.comments.length + " comment" + (draft.comments.length === 1 ? "" : "s");
  const summary = document.getElementById("summary");
  if (summary.value !== draft.summary) summary.value = draft.summary;
}}

// Keep the reader's place across a re-render: the file whose header was at
// or above the top of the viewport is scrolled back to the top when it is
// still there; otherwise the raw offset is restored.
function renderKeepingScroll() {{
  const root = document.getElementById("diff");
  const top = root.scrollTop;
  const files = Array.from(root.querySelectorAll(".file"));
  const above = files.filter((f) => f.offsetTop <= top);
  const topPath = above.length ? above[above.length - 1].dataset.path : null;
  render();
  const again = topPath === null ? null : Array.from(root.querySelectorAll(".file")).find((f) => f.dataset.path === topPath);
  root.scrollTop = again ? again.offsetTop : top;
}}

function fmtAge(ms) {{
  const s = Math.floor(ms / 1000);
  if (s < 1) return "just now";
  if (s < 60) return s + " s ago";
  return Math.floor(s / 60) + " min ago";
}}
function tickAge() {{
  const age = document.getElementById("age");
  age.textContent = renderedAt === null ? "" : "· updated " + fmtAge(Date.now() - renderedAt);
}}
function flashAge() {{
  const age = document.getElementById("age");
  age.classList.add("flash");
  setTimeout(() => age.classList.remove("flash"), 200);
}}

function applyDiff(d, fp) {{
  diff = d; rendered = fp; pendingDiff = null; pending = null;
  lastApplied = Date.now(); renderedAt = lastApplied;
  renderKeepingScroll(); tickAge(); flashAge();
}}
// Spec D PD-3: a waiting diff is applied when no comment box is open and
// at least APPLY_MIN_MS have passed since the last apply; the next poll or
// the next Save/Cancel tries again otherwise.
function maybeApply() {{
  if (pendingDiff && !boxOpen() && Date.now() - lastApplied >= APPLY_MIN_MS) applyDiff(pendingDiff.diff, pendingDiff.fp);
}}

async function fetchDiff() {{
  const r = await fetch(prefix + "/agents/" + id + "/diff.json");
  if (!r.ok) throw new Error(await r.text());
  return await r.json();
}}
// Manual "Reload diff": unconditional, and the way a daemon refusal reaches
// the banner.
async function loadDiff() {{
  const banner = document.getElementById("banner"); banner.textContent = "";
  try {{ applyDiff(await fetchDiff(), latestFp); }}
  catch (e) {{ banner.style.color = "#b00"; banner.textContent = "diff: " + e.message; }}
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

let lastSeq = 0, unread = 0, fetching = false;
const side = document.getElementById("side");
function renderEvent(e) {{
  const row = el("div", "ev" + (e.name === "review_sent" ? " divider" : ""));
  row.append(el("span", "t", new Date(e.at * 1000).toLocaleTimeString()), el("span", "n", e.name), el("span", "s", e.summary));
  const pre = el("pre", "", JSON.stringify(e.payload, null, 2) + (e.payload_truncated ? "\n(truncated)" : "")); pre.hidden = true;
  row.appendChild(pre); row.onclick = () => {{ pre.hidden = !pre.hidden; }};
  return row;
}}
// One poll drives both columns (Spec D PD-2): the events, and the
// worktree's version — a new fingerprint fetches the diff once and parks
// it until maybeApply lets it through.
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
    const w = r.workspace;
    if (w) {{
      latestFp = w.fingerprint;
      if (w.fingerprint !== rendered && !(pendingDiff && pendingDiff.fp === w.fingerprint) && !fetching) {{
        fetching = true;
        try {{ pendingDiff = {{ diff: await fetchDiff(), fp: w.fingerprint }}; }}
        catch (e) {{ console.warn("diff", e); }}
        finally {{ fetching = false; }}
      }}
    }} else if (diff === null && !fetching) {{
      // no version: surface the daemon's reason once
      fetching = true; try {{ await loadDiff(); }} finally {{ fetching = false; }}
    }}
    maybeApply();
  }} catch (e) {{ console.warn("events", e); }}
}}

function applyCollapse() {{ side.classList.toggle("collapsed", !!draft.collapsed); if (!draft.collapsed) {{ unread = 0; document.getElementById("unread").textContent = ""; }} }}
document.getElementById("collapse").onclick = () => {{ draft.collapsed = !draft.collapsed; save(); applyCollapse(); }};
document.getElementById("reload").onclick = loadDiff;
document.getElementById("send").onclick = send;
document.getElementById("summary").oninput = (ev) => {{ draft.summary = ev.target.value; save(); }};
applyCollapse();
render();
pollEvents();
setInterval(pollEvents, {poll});
setInterval(tickAge, 1000);
```

Notes: the first poll carries the fingerprint, so the first diff is fetched and applied at once (`lastApplied` starts at 0). `loadDiff` on the no-version path shows the daemon's refusal in the banner and leaves `diff` null, so it retries on later polls only while `diff === null` — a page opened before `up` materialized the worktree recovers by itself. `save()` strips `editing` and `typing`, closing an earlier deferred minor (an in-progress edit was persisted).

- [ ] **Step 4: Check the script**

Write `review_html("", "f/c/a")` to `/tmp/claude-1000/-workspace/f99efab9-2435-494c-81ae-311615f3d4bf/scratchpad/review-d.html` from a throwaway test (delete it afterwards), extract the last `<script>` body to `review-d.js`, and run `node --check review-d.js` (node is on PATH via mise's installs even though it is not a `mise.toml` tool; if it is absent, skip and say so). Then run this scratch script with `node` to exercise `anchorComments` (paste the `parsePatch`, `anchorOf` and `anchorComments` definitions above it, with the doubled braces undoubled):

```js
const diff = { files: [{ path: "a", patch: "@@ -1,2 +1,3 @@\n line\n+new\n+old\n" }] };
const c1 = { path: "a", side: "new", line: 3, text: "+old", body: "x" };      // exact: line 3 is "+old"
const c2 = { path: "a", side: "new", line: 9, text: "+new", body: "y" };      // moved: unique "+new" at line 2
const c3 = { path: "a", side: "new", line: 9, text: "+gone", body: "z" };     // vanished
const dup = { files: [{ path: "a", patch: "@@ -1 +1,2 @@\n+same\n+same\n" }] };
const c4 = { path: "a", side: "new", line: 9, text: "+same", body: "w" };     // ambiguous
const stale = anchorComments(diff, [c1, c2, c3]);
console.log(c1.line === 3 && c2.line === 2 && stale.length === 1 && stale[0] === c3 ? "ok1" : "FAIL1");
console.log(anchorComments(dup, [c4]).length === 1 && c4.line === 9 ? "ok2" : "FAIL2");
```

Expected: `ok1`, `ok2`. Record the output in the report. Then take a headless screenshot as Spec C's Task 7 did (`chromium --headless=new --no-sandbox --screenshot=… file://…review-d.html`) and confirm the header shows the age span (empty before a diff) — the fetches fail from `file://`, which is fine.

Run: `mise x -- cargo test -p balerix-plugin-web`, clippy for the crate, fmt.
Expected: PASS.

- [ ] **Step 5: Commit**

Subject: `Refresh the review page's diff live from the polled version`. Body: Spec D §3.2 — one poll, auto-apply rate limited to 3 s, deferred while a comment box is open, comments re-anchored by quoted text, scroll kept, the indicator; the first diff from the first poll (Spec D §11). Trailer as in Global Constraints.

---

### Task 7: The e2e, `verify-claude`, the docs, Spec D §7 and §11

**Files:**
- Modify: `crates/balerix/tests/e2e.rs` (`web_journey`, after the `PreToolUse` events assertion and before the review POST)
- Modify: `scripts/verify-claude.sh` (`browser_login`, after the review lines)
- Modify: `docs/THREAT-MODEL.md`, `ARCHITECTURE.md`, `AGENTS.md`, `README.md`, Spec D (§7 verdicts, new §11)

- [ ] **Step 1: The e2e**

In `web_journey`, after the block asserting `PreToolUse` is in `events.json` (it ends with `"{events}"` in an `assert!`), add:

```rust
    // the version rides on events.json and changes when the worktree does
    let fp0 = events["workspace"]["fingerprint"]
        .as_str()
        .unwrap_or_else(|| panic!("workspace in {events}"))
        .to_string();
    assert_eq!(fp0.len(), 64);
    assert_eq!(events["workspace"]["head"], diff["head"]);
    fs::write(w.agent_dir("alice").join("workspace/NOTES.md"), "agent notes\nmore\n").unwrap();
    let (_, _, body) = raw_get(
        &format!("{mount}agents/e2e/c/alice/events.json"),
        &[("Cookie", &cookie)],
    );
    let again: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_ne!(again["workspace"]["fingerprint"], fp0, "{again}");
    let (_, _, body) = raw_get(
        &format!("{mount}agents/e2e/c/alice/diff.json"),
        &[("Cookie", &cookie)],
    );
    let diff2: serde_json::Value = serde_json::from_str(&body).unwrap();
    let notes2 = diff2["files"].as_array().unwrap().iter().find(|f| f["path"] == "NOTES.md").unwrap();
    assert!(notes2["patch"].as_str().unwrap().contains("+more"), "{notes2}");
```

(`events` is the `serde_json::Value` bound by the existing block; `diff` the earlier `diff.json` value. The later review POST quotes `+agent notes` on line 1, which is still line 1 of the two-line file, so the stdin assertions hold.) In the metrics wait, add `&& m.contains("balerix_plugin_web_diff_refreshes_total")`.

Run: `mise x -- cargo nextest run -p balerix --test e2e web_journey` (after `mise run package-plugins`).
Expected: PASS.

- [ ] **Step 2: `verify-claude`**

In `scripts/verify-claude.sh`, after the three `say ">>> …review…"` lines added by Spec C, add:

```sh
    say ">>> Spec D: with the review page open, edit a file in the agent's worktree (or let"
    say ">>> the agent do it): the diff updates within a few seconds and 'updated N s ago'"
    say ">>> flashes; with a comment box open the update waits until Save or Cancel."
```

Run `BALERIX_VERIFY_FAKE=1 mise run verify-claude` to see the lines print.

- [ ] **Step 3: The docs**

- `docs/THREAT-MODEL.md`: the Plugin ↔ daemon route list already says `agents/*/workspace/*` (covers `version`); in the workspace mitigation row (the one starting "A workspace read escaping the worktree"), append to the mitigation cell ", `version` runs the same hardened name-only calls and emits a hash, never content".
- `ARCHITECTURE.md`: in the Spec C paragraph of "How it flows", after "committed, uncommitted and untracked alike." add "A fourth route, `version`, answers a cheap fingerprint of the worktree (Spec D) that the review page polls through `events.json` to refresh the diff live."
- `AGENTS.md` gotchas, after the two-404s gotcha:

```
- The workspace `version` fingerprint covers `HEAD`, the merge-base and the
  size and mtime of every changed or untracked path — content, not index
  state: a byte-identical `git add` is invisible by design, and a same-size
  rewrite within the filesystem's mtime resolution is the theoretical miss.
  The review page applies a changed diff automatically (3 s rate limit),
  but never while a comment box is open.
```

- `README.md`: after the "Upgrading to Spec C" section:

```
### Upgrading to Spec D
- `GET agents/…/workspace/version` is a fourth `workspace` route
  (plugin-protocol §3); `Host::workspace_version` is new; nothing existing
  changed. The web plugin's `events.json` gains a `workspace` field; re-run
  `mise run package-plugins`.
```

- Spec D §7: fill the table's two rows with the verdicts ("Verified 2026-09-08 (inspect_it: an edit within the same second changed the fingerprint)" if Task 2's edit case passed without the fallback; "Pending — by-hand `verify-claude`" for the scroll feel). Append:

```
## 11. Refinements from the plan (2026-09-08)

- **`diff_refreshes_total` counts `diff.json` responses**, the one server-side event per refresh; the browser's applies are not observable.
- **The fakes' fingerprint** is `sha256(head \0 (path \0 bytes \0)*)` over the file map, in core and in the SDK alike, so `workspace-version.json` holds a derivable value.
- **The first diff comes from the first poll**, which carries the fingerprint; `loadDiff()` is the manual reload and the path that shows a daemon refusal in the banner.
- **`anchorComments` is checked with `node` by hand** (not a `mise.toml` tool); the Rust test asserts the function's presence.
- **`save()` strips `editing` and `typing`**, so a reload never reopens a comment box (a Spec C deferred minor).
```

- [ ] **Step 4: Run everything and commit**

Run: `mise run check`, `mise run test-it`, `mise run e2e`.
Expected: all green.

Subject: `Close Spec D: the e2e, verify-claude, the docs and §11`. Trailer as in Global Constraints. Then the branch is ready for the finishing skill (merge or PR against `main` is the user's call; `spec-c-workspace-review` is PR #36, so this branch's PR should target it or wait for it).
