# Spec N: A Private Clone per Agent Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every agent its own `git clone --reference` of the repository instead of a worktree off a shared crew clone, turn the crew clone into a daemon-owned, append-only object cache agents read but never write, and harvest an agent's branch into that cache before its clone is deleted.

**Architecture:** All the change is in `balerix-runtime`. `Workspace::ensure_worktree` becomes `ensure_clone` (same marker rule as Spec L §12, now over a private clone; a 0.1.x worktree is refused), `remove_worktree` becomes `harvest_and_remove` (a fetch run in the cache, never a push from the clone), `ensure_repo` pins `gc.auto=0`, and the sandbox grants the cache's `objects/` read-only with nothing under `repo/` in `allow`. The pure clone decision (`decide_clone`) and a shared hardened-git builder (`harden_agent_git`, also used by `inspect.rs`) are unit-tested without git; everything else is `workspace_it`, `sandbox_it`, `materialize_it` and `inspect_it` against real git and nono. No port, wire type or plugin changes.

**Tech Stack:** Rust 1.98 (edition 2024), real `git` and `nono` in the `balerix-runtime` integration tests (`mise run test-it`), insta for the one golden snapshot that moves, the e2e under `target/tmp`.

**Spec:** `docs/superpowers/specs/2026-09-25-balerix-n-private-clones-design.md`

## Global Constraints

- Run cargo through mise: `mise x -- cargo …`, or `mise run <task>`. `mise run check` (lint + test) must pass at the end of every task; `mise run test-it` at the end of Tasks 1–4; `mise run e2e` at the end of Task 6.
- Integration tests skip with a printed reason when a tool is missing; run them with `BALERIX_REQUIRE_TOOLS=1` (what `mise run test-it` sets) so a skip is a failure. Every test root is under `target/tmp` (`support::temp_root`), never `/tmp`: nono grants `/tmp` by default, and an escape assertion there would pass vacuously.
- Library crates return `thiserror` errors whose messages start with the id (`f/c/a: …`); `MaterializeError` has no new variant — `Invalid` carries the two new messages, `Tool` carries git's.
- `unsafe_code = "forbid"`; clippy `all` warn, `unwrap_used`/`expect_used` warn outside tests. Never `std::env::set_var`.
- No new dependency in this plan.
- Every git call on an agent-writable repository (an existing clone) carries the hardening the workspace reader uses: `-c core.fsmonitor=false`, `-c core.hooksPath=<empty dir>`, `GIT_OPTIONAL_LOCKS=0`, `GIT_TERMINAL_PROMPT=0`, the five repo-locating `GIT_*` variables scrubbed, `GIT_CEILING_DIRECTORIES` at the agent's canonical root (spec §4 step 1, #62).
- The daemon never writes into an agent's clone once it exists, and never runs a push from it. Every daemon fetch into the cache is `fetch --quiet --no-auto-gc`; the cache is created with `gc.auto=0` and never pruned (N-2, N-5).
- Agents get exactly one path under `crews/<c>/repo`: `.git/objects`, in `read`. Nothing under `repo/` is ever in `allow` (N-3).
- insta snapshots: read the `.snap.new`, compare against the expected values written in the task, then `mise x -- cargo insta accept`. Never blind-accept. Exactly one snapshot changes in this plan (`generated_golden__payments_generated.snap`, Task 1), by exactly the two edits Task 1 names.
- Commit after every task. The final PR title is `feat(runtime)!: a private clone per agent (Spec N)`; the `!` is the whole breaking signal to git-cliff (squash merges use the title alone), and it bumps the core unit to 0.2.0. The PR touches nothing under `crates/balerix-api/` or `crates/balerix-plugin-sdk/`, so it releases core only.
- `mise.toml` carries an unrelated uncommitted edit (`claude = "2.1.282"`). Leave it out of every commit in this plan.

## Review Focus

1. **A clone whose creation fails after `git clone`** (a `branch` whose `origin/<start_ref>` the remote lacks) must leave no half-made clone behind: a `--no-checkout` clone reads as an existing, dirty clone on the next pass and would turn a clear git error into "the clone has local changes" forever. Test: `a_missing_remote_branch_fails_the_clone_and_leaves_nothing` in Task 3.
2. **A 0.1.x crew repo kept by `down --keep-repos`** has no `gc.auto=0` and stale worktree registrations; used as the cache it must stay safe (every daemon fetch carries `--no-auto-gc`) and the branches it holds must seed the new clones, so the unpushed work in it is not lost. Test: `a_worktree_from_0_1_is_refused_and_keep_repos_migrates_it` in Task 4.
3. **Removal with no marker, a detached HEAD, or a branch the agent deleted** must still remove the agent: the branch HEAD is on is harvested when there is no marker; nothing is harvested for a detached HEAD or a missing branch, and the removal succeeds. Test: `removal_harvests_head_without_a_marker_and_skips_what_is_not_there` in Task 4.
4. **`down --purge` over a clone git cannot read** must succeed: when the cache is going too there is nothing to harvest into, so a broken clone cannot wedge a purge. Test: `a_purge_deletes_a_broken_clone_without_harvesting` in Task 4.
5. **Repo-local config in the clone must not run under the daemon** during the clone step's `status` on a branch change (`core.fsmonitor`, the first bullet of #62). Test: `the_clone_step_runs_no_program_from_the_clone_config` in Task 3.

---

## File Structure

**Modified**

- `crates/balerix-runtime/src/layout.rs` — `CrewPaths::cache_objects()`; the `branch_marker` doc comment.
- `crates/balerix-runtime/src/sandbox.rs` — `balerix_grants`: `allow` is `[home, workspace]`, `read` gains the cache's `objects/`.
- `crates/balerix-runtime/src/workspace.rs` — `scrub_git_env`, `harden_agent_git`, `decide_clone`/`CloneDecision`, `ensure_repo` (gc.auto), `ensure_clone` (replaces `ensure_worktree`), `harvest`, `harvest_and_remove` (replaces `remove_worktree`).
- `crates/balerix-runtime/src/inspect.rs` — `inspect_git` built on `harden_agent_git`; `CONFIG` loses the fsmonitor entry the helper now supplies.
- `crates/balerix-runtime/src/materializer.rs` — the three call sites (`materialize`, `remove_agent`, `remove_crew`).
- `crates/balerix-runtime/tests/{workspace_it,sandbox_it,materialize_it,inspect_it,generated_golden}.rs` and `tests/snapshots/generated_golden__payments_generated.snap`.
- `crates/balerix-api/src/branch.rs`, `crates/balerix-core/src/agent.rs` — doc comments naming the clone.
- `docs/THREAT-MODEL.md`, `ARCHITECTURE.md`, `AGENTS.md`, `CHANGELOG.md`, `docs/RELEASING.md`, the Spec M and Spec L files, the Spec N file (§12 "Recorded at implementation").

Nothing is created; nothing in `balerix-core`, `balerix-api`, `balerix-server` or the plugins changes behaviour.

---

### Task 1: The cache's `objects/` is the only shared path (layout + sandbox)

**Files:**
- Modify: `crates/balerix-runtime/src/layout.rs:72-82` (`impl CrewPaths`), `:240-247` (`branch_marker` doc)
- Modify: `crates/balerix-runtime/src/sandbox.rs:23-61` (`balerix_grants`), `:305-337` (the grants test)
- Modify: `crates/balerix-runtime/tests/sandbox_it.rs:10-94`
- Modify: `crates/balerix-runtime/tests/snapshots/generated_golden__payments_generated.snap` (via `cargo insta accept`)

**Interfaces:**
- Produces: `CrewPaths::cache_objects(&self) -> PathBuf` (= `repo/.git/objects`), used by Task 3's tests and the threat model. `balerix_grants` signature unchanged.

- [ ] **Step 1: Write the failing layout test**

In `crates/balerix-runtime/src/layout.rs`, inside `mod tests`, add to `agent_paths_follow_the_spec_layout` after the `l.crew(...).repo` assertion:

```rust
        assert_eq!(
            l.crew(&"payments/backend".parse().unwrap()).cache_objects(),
            PathBuf::from(
                "/h/.local/state/balerix/fleets/payments/crews/backend/repo/.git/objects"
            ),
            "Spec N-3: the one path under repo/ an agent can see"
        );
```

- [ ] **Step 2: Run it to see it fail**

Run: `mise x -- cargo test -p balerix-runtime --lib layout::tests::agent_paths_follow_the_spec_layout`
Expected: FAIL, `no method named cache_objects`.

- [ ] **Step 3: Add `cache_objects` and reword the marker doc**

In `impl CrewPaths`:

```rust
    /// The object cache the agents' private clones borrow from (Spec N-3):
    /// the only path under `repo/` an agent's sandbox sees, read-only. An
    /// `objects/info/alternates` names an `objects/` directory, not a
    /// repository, so nothing else in the cache is needed by the agent's git.
    pub fn cache_objects(&self) -> PathBuf {
        self.repo.join(".git").join("objects")
    }
```

Replace the `branch_marker` doc comment:

```rust
    /// Holds the branch balerix last created the clone on. A changed agent
    /// `branch` is detected against this, never against the clone's HEAD,
    /// so an agent that checked out a branch of its own is left on it
    /// across restarts (#60); at removal it names the branch to harvest
    /// into the cache (Spec N §5). Daemon-owned: the agent root is outside
    /// every sandbox grant.
```

- [ ] **Step 4: Run the layout test to see it pass**

Run: `mise x -- cargo test -p balerix-runtime --lib layout::tests`
Expected: PASS.

- [ ] **Step 5: Write the failing grants test**

In `crates/balerix-runtime/src/sandbox.rs` `mod tests`, in `base_profile_has_the_required_grants_env_and_port`, replace the two assertions after the `/opt/mise` one (`len() == 11` and `allow[2]`) with:

```rust
        assert_eq!(
            p["filesystem"]["read"][11],
            "/h/.local/state/balerix/fleets/f/crews/c/repo/.git/objects",
            "Spec N-3: the crew's object cache, read-only"
        );
        assert_eq!(p["filesystem"]["read"].as_array().unwrap().len(), 12);
        assert_eq!(
            p["filesystem"]["allow"],
            json!([
                "/h/.local/state/balerix/fleets/f/crews/c/agents/a/home",
                "/h/.local/state/balerix/fleets/f/crews/c/agents/a/workspace",
            ]),
            "nothing under crews/c/repo is ever writable (Spec N §6)"
        );
```

- [ ] **Step 6: Run it to see it fail**

Run: `mise x -- cargo test -p balerix-runtime --lib sandbox::tests::base_profile_has_the_required_grants_env_and_port`
Expected: FAIL on `read[11]` (null) or the `allow` comparison (three entries).

- [ ] **Step 7: Change the grants**

In `balerix_grants`, replace the `read.push(std::fs::canonicalize(mise)...)` line through the closing `}` of the function with:

```rust
    read.push(std::fs::canonicalize(mise).unwrap_or_else(|_| mise.to_path_buf()));
    // Spec N-3: the crew's object cache, read-only. The agent's private
    // clone borrows objects from it through `objects/info/alternates`; a
    // Landlock read rule on the directory covers the tree beneath it. No
    // path under `repo/` is ever in `allow` — that is the isolation between
    // the agents of one crew (docs/THREAT-MODEL.md).
    read.push(crew.cache_objects());
    Grants {
        read,
        allow: vec![paths.home.clone(), paths.workspace.clone()],
    }
```

- [ ] **Step 8: Run the sandbox unit tests**

Run: `mise x -- cargo test -p balerix-runtime --lib sandbox::tests`
Expected: PASS.

- [ ] **Step 9: Update the golden snapshot, by inspection**

Run: `mise x -- cargo test -p balerix-runtime --test generated_golden`
Expected: FAIL with a snapshot diff. Open `crates/balerix-runtime/tests/snapshots/generated_golden__payments_generated.snap.new` and confirm the diff is exactly, for each of the two agents' profiles: the `"<root>/state/fleets/payments/crews/backend/repo/.git"` entry removed from `allow`, and `"<root>/state/fleets/payments/crews/backend/repo/.git/objects"` appended to `read` after the mise entry. Nothing else. Then:

Run: `mise x -- cargo insta accept` and re-run the golden test.
Expected: PASS.

- [ ] **Step 10: Write the failing `sandbox_it` case**

In `crates/balerix-runtime/tests/sandbox_it.rs`, `generated_profile_validates_and_enforces_isolation`: replace `crew.repo.join(".git"),` in `dirs` with `crew.cache_objects(),`; after the `for d in &dirs` loop add:

```rust
    // Spec N-3: the cache is readable and not writable from inside.
    let objects = crew.cache_objects();
    std::fs::write(objects.join("probe"), "probe-ok\n").unwrap();
```

Replace the `script` with:

```rust
    let script = format!(
        "echo in > \"$HOME/ok\" && echo HOME=$HOME && echo FOO=$FOO \
         && (echo x > {outside}/nope 2>/dev/null && echo ESCAPED || echo denied) \
         && (cat {objects}/probe 2>/dev/null || echo CACHE_UNREADABLE) \
         && (echo x > {objects}/nope 2>/dev/null && echo CACHE_WRITABLE || echo cache-denied)",
        outside = outside.display(),
        objects = objects.display()
    );
```

After the `assert!(!outside.join("nope").exists());` line add:

```rust
    assert!(
        stdout.contains("probe-ok"),
        "an object under the cache reads from inside the profile: {stdout}"
    );
    assert!(
        stdout.contains("cache-denied") && !stdout.contains("CACHE_WRITABLE"),
        "a write into the cache must be denied: {stdout}"
    );
    assert!(!objects.join("nope").exists());
```

- [ ] **Step 11: Run `sandbox_it`**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --test sandbox_it`
Expected: PASS (both tests). If `landlock` is reported unavailable on this host the test skips and `BALERIX_REQUIRE_TOOLS` fails it — that is a host problem, not a code one; `mise run test-it` in CI is the gate.

- [ ] **Step 12: Full gate and commit**

Run: `mise run check && mise run test-it`
Expected: both PASS. Then:

```bash
git add crates/balerix-runtime/src/layout.rs crates/balerix-runtime/src/sandbox.rs crates/balerix-runtime/tests/sandbox_it.rs crates/balerix-runtime/tests/snapshots/generated_golden__payments_generated.snap
git commit -m "feat(runtime): grant agents the crew object cache read-only, nothing under repo/ writable (Spec N §6)"
```

---

### Task 2: The hardened git builder and the pure clone decision

**Files:**
- Modify: `crates/balerix-runtime/src/workspace.rs` (new free functions and their unit tests; `Workspace::git` uses `scrub_git_env`)
- Modify: `crates/balerix-runtime/src/inspect.rs:24-32` (`CONFIG`), `:76-137` (`inspect_git`)

**Interfaces:**
- Produces: `pub(crate) fn scrub_git_env(cmd: Cmd) -> Cmd`; `pub(crate) fn harden_agent_git(cmd: Cmd, crew: &CrewPaths, agent_root: &Path) -> Cmd`; `pub enum CloneDecision { Reuse, Record, Recreate { old: String }, Dirty { old: String } }`; `pub fn decide_clone<E>(marker: Option<&str>, head: Option<&str>, branch: &str, dirty: impl FnOnce() -> Result<bool, E>) -> Result<CloneDecision, E>`. Task 3 calls all four.

- [ ] **Step 1: Write the failing decision tests**

At the bottom of `crates/balerix-runtime/src/workspace.rs` add:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn clean() -> Result<bool, ()> {
        Ok(false)
    }
    fn never() -> Result<bool, ()> {
        panic!("the tree is not inspected on this path")
    }

    /// Spec L §12's marker rule over the clone (Spec N §4 step 1).
    #[test]
    fn a_matching_marker_reuses_whatever_head_is() {
        assert_eq!(
            decide_clone(Some("b"), Some("my-fix"), "b", never),
            Ok(CloneDecision::Reuse)
        );
        assert_eq!(
            decide_clone(Some("b\n"), None, "b", never),
            Ok(CloneDecision::Reuse),
            "the marker file ends in whatever write_atomic wrote; trimmed"
        );
    }

    #[test]
    fn a_detached_head_is_reused_as_is() {
        assert_eq!(
            decide_clone(Some("old"), None, "b", never),
            Ok(CloneDecision::Reuse)
        );
        assert_eq!(decide_clone(None, None, "b", never), Ok(CloneDecision::Reuse));
    }

    #[test]
    fn a_head_already_on_the_branch_is_recorded_without_a_move() {
        assert_eq!(
            decide_clone(Some("old"), Some("b"), "b", never),
            Ok(CloneDecision::Record)
        );
        assert_eq!(
            decide_clone(None, Some("b"), "b", never),
            Ok(CloneDecision::Record),
            "no marker: judged by HEAD once"
        );
    }

    #[test]
    fn another_branch_moves_a_clean_clone_and_fails_a_dirty_one() {
        assert_eq!(
            decide_clone(Some("old"), Some("old"), "b", clean),
            Ok(CloneDecision::Recreate { old: "old".into() })
        );
        assert_eq!(
            decide_clone(Some("old"), Some("my-fix"), "b", || Ok::<_, ()>(true)),
            Ok(CloneDecision::Dirty { old: "old".into() }),
            "the marker names the old branch, not the one the agent is on"
        );
        assert_eq!(
            decide_clone(None, Some("my-fix"), "b", clean),
            Ok(CloneDecision::Recreate { old: "my-fix".into() }),
            "no marker: HEAD is the old branch"
        );
    }

    #[test]
    fn a_failed_dirty_check_propagates() {
        assert_eq!(
            decide_clone(Some("old"), Some("old"), "b", || Err("status failed")),
            Err("status failed")
        );
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `mise x -- cargo test -p balerix-runtime --lib workspace::tests`
Expected: FAIL to compile, `cannot find function decide_clone`.

- [ ] **Step 3: Add the decision and the two builders**

Change the module doc and imports at the top of `workspace.rs`:

```rust
//! Crew object cache and per-agent private clone (Spec N; Phase 2 spec
//! §4.2 step 1 before it).

use std::path::{Path, PathBuf};

use balerix_core::{MaterializeError, RepoRef};

use crate::fsutil::write_atomic;
use crate::home::render_hosts_yml;
use crate::layout::{AgentPaths, CrewPaths};
use crate::tools::{Cmd, ToolPaths};
```

Add, above `pub struct Workspace`:

```rust
/// `git` honours `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_PREFIX`
/// and `GIT_COMMON_DIR` from the environment over an explicit `-C`: if any
/// of these leak in (a pre-commit hook exports them, and so does a daemon
/// started under one), every `-C` call would silently operate on whatever
/// repository those variables name. Scrubbed from every git call balerix
/// makes.
pub(crate) fn scrub_git_env(mut cmd: Cmd) -> Cmd {
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_PREFIX",
        "GIT_COMMON_DIR",
    ] {
        cmd = cmd.env_remove(var);
    }
    cmd
}

/// Config and environment for a git call in a repository the agent can
/// write to (its private clone), shared by the workspace reader
/// (`inspect.rs`) and the clone step (Spec N §4 step 1, #62). Command-line
/// config beats every config file, so nothing the agent wrote into its
/// `.git/config` runs as the daemon: fsmonitor off, hooks pointed at an
/// empty directory, no optional locks, no prompt, and the `GIT_*` scrub.
/// `GIT_CEILING_DIRECTORIES` is the agent's own root, the parent of
/// `workspace/`: the agent owns the clone and can delete its `.git`, and
/// repository discovery would then walk up and run the command in whatever
/// repository contains the state root. git only honours a ceiling that
/// matches the resolved path, so it is canonical.
pub(crate) fn harden_agent_git(cmd: Cmd, crew: &CrewPaths, agent_root: &Path) -> Cmd {
    let no_hooks = crew.root.join("no-hooks");
    let _ = std::fs::create_dir_all(&no_hooks);
    let ceiling = agent_root
        .canonicalize()
        .unwrap_or_else(|_| agent_root.to_path_buf());
    scrub_git_env(cmd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CEILING_DIRECTORIES", ceiling.display().to_string())
        .args(["-c", "core.fsmonitor=false"])
        .args([
            "-c".to_string(),
            format!("core.hooksPath={}", no_hooks.display()),
        ])
}

/// What to do with an existing clone (Spec N §4 step 1): Spec L §12's
/// marker rule, as a function of the marker, the clone's HEAD (`None`
/// when detached) and the configured `branch`. `dirty` is consulted only
/// when the clone would have to move, so the git call behind it is not
/// made on the common path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloneDecision {
    /// The marker matches, or HEAD is detached: leave the clone as it is.
    Reuse,
    /// HEAD already sits on `branch`: write the marker, move nothing.
    Record,
    /// A clean clone on another branch: harvest `old`, delete, re-create.
    Recreate { old: String },
    /// A dirty clone on another branch: fail, naming `old`.
    Dirty { old: String },
}

pub fn decide_clone<E>(
    marker: Option<&str>,
    head: Option<&str>,
    branch: &str,
    dirty: impl FnOnce() -> Result<bool, E>,
) -> Result<CloneDecision, E> {
    let marker = marker.map(str::trim);
    if marker == Some(branch) {
        return Ok(CloneDecision::Reuse);
    }
    let Some(head) = head.map(str::trim) else {
        return Ok(CloneDecision::Reuse);
    };
    if head == branch {
        return Ok(CloneDecision::Record);
    }
    let old = marker.unwrap_or(head).to_string();
    Ok(if dirty()? {
        CloneDecision::Dirty { old }
    } else {
        CloneDecision::Recreate { old }
    })
}
```

In `Workspace::git`, replace the `for var in [...] { cmd = cmd.env_remove(var); }` loop and the doc comment above the method with:

```rust
    /// One git call as the daemon, logged to the crew's `git.log`, with
    /// the gh credential helper when `git.auth: gh` (`scrub_git_env` on
    /// every call). For the cache and for a clone at creation; a call in
    /// an existing clone goes through `agent_git`.
    fn git(&self, id: &str, crew: &CrewPaths, args: &[&str]) -> Result<String, MaterializeError> {
        let mut cmd = scrub_git_env(
            Cmd::new(&self.tools.git).log(&crew.root.join("logs").join("git.log")),
        );
```

(keep the rest of the method as it is). Re-export the decision from `crates/balerix-runtime/src/lib.rs` (`harden_agent_git` and `scrub_git_env` stay `pub(crate)`):

```rust
pub use workspace::{CloneDecision, Workspace, decide_clone};
```

- [ ] **Step 4: Run the decision tests**

Run: `mise x -- cargo test -p balerix-runtime --lib workspace::tests`
Expected: PASS (5 tests).

- [ ] **Step 5: Build `inspect_git` on the shared helper**

In `crates/balerix-runtime/src/inspect.rs`, remove `"-c", "core.fsmonitor=false",` from `CONFIG` (the helper supplies it) and reword its doc: `/// Command-line config beats every config file: whatever an agent wrote into its clone's `.git/config`, no program runs from it here (the rest of the hardening is `harden_agent_git`).` Replace the body of `inspect_git` from `let no_hooks = …` through the `.args(args.iter().copied());` line with:

```rust
        let cmd = crate::workspace::harden_agent_git(
            Cmd::new(&self.tools.git).log_argv_only(&crew.root.join("logs").join("git.log")),
            crew,
            &paths.root,
        )
        .args(CONFIG.iter().copied())
        .args(["-C".to_string(), paths.workspace.display().to_string()])
        .args(args.iter().copied());
```

Shorten the method's doc comment to say the hardening is `harden_agent_git`'s and keep the sentence about `log_argv_only` (the diff volume). Update the module doc's first lines: "the agent's clone against the crew's base … an agent can write its clone's `.git/config` and `.gitattributes`".

- [ ] **Step 6: Run the reader's integration test**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --test inspect_it`
Expected: PASS — `inspect_it` plants an fsmonitor script and a nested `diff.external` and asserts neither ran, and deletes the `.git` file to prove the ceiling; all of that now goes through the shared helper.

- [ ] **Step 7: Gate and commit**

Run: `mise run check`
Expected: PASS.

```bash
git add crates/balerix-runtime/src/workspace.rs crates/balerix-runtime/src/inspect.rs crates/balerix-runtime/src/lib.rs
git commit -m "refactor(runtime): share the hardened git builder and make the clone decision pure (Spec N §4)"
```

---

### Task 3: `ensure_clone` — the private clone, seeded from the cache

**Files:**
- Modify: `crates/balerix-runtime/src/workspace.rs` (`ensure_repo`, `ensure_worktree` → `ensure_clone`, new `agent_git`, `create_clone`, `harvest`, `remove_tree`, `record_branch`; `is_registered` deleted)
- Modify: `crates/balerix-runtime/src/materializer.rs:334-354` (`materialize`)
- Modify: `crates/balerix-runtime/tests/workspace_it.rs` (rewritten around the clone; the removal cases come in Task 4)
- Modify: `crates/balerix-runtime/tests/inspect_it.rs:88-97` (the fixture call), `:426-437` (the `.git` deletion)

**Interfaces:**
- Consumes: `harden_agent_git`, `scrub_git_env`, `decide_clone`, `CloneDecision` (Task 2); `CrewPaths::cache_objects` (Task 1).
- Produces: `Workspace::ensure_clone(&self, id: &str, crew: &CrewPaths, agent: &AgentPaths, repo: &RepoRef, branch: &str, start_ref: &str) -> Result<(), MaterializeError>`; `Workspace::ensure_repo` unchanged in signature, now sets `gc.auto=0`; private `Workspace::harvest(&self, id, crew, agent, branch) -> Result<(), MaterializeError>` and `fn remove_tree(id: &str, path: &Path) -> Result<(), MaterializeError>` that Task 4 reuses. `remove_worktree` and the `is_registered` it uses stay until Task 4 deletes both (on a clone `remove_worktree` finds no registered worktree and only prunes).

- [ ] **Step 1: Rewrite `workspace_it` for the clone (this task's cases)**

Replace `crates/balerix-runtime/tests/workspace_it.rs` from `#[test] fn clone_worktree_reuse_and_remove` to the end of the file with the tests below. Keep the `git`, `bare_repo` and `push_branch` helpers, and keep `errors_name_the_id_tool_and_first_stderr_line` and `check_branch_name_agrees_with_git_check_ref_format` exactly as they are (they are not repeated here). Add two helpers after `push_branch`:

```rust
/// The `$ git …` lines of the crew's git.log that run a `fetch`.
fn fetch_lines(crew: &balerix_runtime::CrewPaths) -> Vec<String> {
    std::fs::read_to_string(crew.root.join("logs").join("git.log"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("$ git") && l.contains(" fetch "))
        .map(str::to_string)
        .collect()
}

/// Spec N-2: every fetch the daemon runs into the cache must refuse to gc.
fn assert_no_auto_gc(crew: &balerix_runtime::CrewPaths) {
    for l in fetch_lines(crew) {
        assert!(l.contains("--no-auto-gc"), "a daemon fetch without --no-auto-gc: {l}");
    }
}
```

The tests:

```rust
/// Spec N §3–4: the cache is a `--no-checkout` clone with `gc.auto=0`;
/// the agent's workspace is a full clone borrowing objects from it, on
/// the agent's branch, with `origin` the real remote.
#[test]
fn a_private_clone_borrows_from_the_cache_and_pushes_to_origin() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };

    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    assert!(crew.repo.join(".git").is_dir());
    assert!(!crew.repo.join("README").exists(), "--no-checkout");
    assert_eq!(
        git(&crew.repo, &["config", "gc.auto"]).trim(),
        "0",
        "N-2: the cache never gcs on its own"
    );
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap(); // present → no git call
    assert_eq!(fetch_lines(&crew).len(), 0, "a second ensure_repo must not fetch");

    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(fetch_lines(&crew).len(), 1, "creating a branch fetches the cache first");
    assert_no_auto_gc(&crew);
    assert!(paths.workspace.join(".git").is_dir(), "a clone, not a worktree");
    assert_eq!(
        std::fs::read_to_string(paths.workspace.join("README")).unwrap(),
        "hi\n"
    );
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "balerix/f/c/a"
    );
    let alternates = std::fs::read_to_string(
        paths.workspace.join(".git/objects/info/alternates"),
    )
    .unwrap();
    assert_eq!(
        alternates.trim(),
        crew.cache_objects().canonicalize().unwrap().display().to_string(),
        "N-1: objects come from the cache"
    );
    assert_eq!(
        git(&paths.workspace, &["remote", "get-url", "origin"]).trim(),
        repo.clone_url(),
        "origin is the real remote, not the cache"
    );
    assert_eq!(
        std::fs::read_to_string(paths.branch_marker()).unwrap(),
        "balerix/f/c/a"
    );

    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap(); // idempotent
    assert_eq!(fetch_lines(&crew).len(), 1, "an existing clone costs no fetch");

    // a push from the clone reaches origin
    std::fs::write(paths.workspace.join("work.txt"), "pushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);
    git(&paths.workspace, &["push", "-q", "origin", "balerix/f/c/a"]);
    let bare = root.join("upstream.git");
    assert_eq!(git(&bare, &["rev-parse", "refs/heads/balerix/f/c/a"]), sha);

    // a second agent gets its own clone and branch from origin/main
    let b = layout.agent(&"f/c/b".parse().unwrap());
    ws.ensure_clone("f/c/b", &crew, &b, &repo, "balerix/f/c/b", "main")
        .unwrap();
    assert!(!b.workspace.join("work.txt").exists());
    assert!(b.workspace.join(".git/objects/info/alternates").exists());
}

/// Spec L §6 over the clone: an agent with `branch` works on that remote
/// branch — created from `origin/<branch>`, tracking it, reused across
/// passes — never on a fresh `balerix/…` branch.
#[test]
fn a_clone_on_an_existing_remote_branch_is_created_from_it_and_reused() {
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

    ws.ensure_clone("f/c/pr", &crew, &paths, &repo, "feature/issue-12", "feature/issue-12")
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
        "a push from the clone reaches the PR's branch"
    );
    ws.ensure_clone("f/c/pr", &crew, &paths, &repo, "feature/issue-12", "feature/issue-12")
        .unwrap();
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]).trim(), tip, "reused");
}

/// Spec L §6 (F1) over the clone: adding, changing or removing `branch`
/// on a live agent re-creates its clean clone on the new branch; the old
/// branch is harvested into the cache (Spec N §5) and seeds the clone
/// when the setting comes back.
#[test]
fn a_changed_branch_moves_a_clean_clone_and_keeps_the_old_branch() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-rebranch");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let tip = push_branch(&root, "feature/issue-12");
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    let head = || {
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"])
            .trim()
            .to_string()
    };

    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(head(), "balerix/f/c/a");
    std::fs::write(paths.workspace.join("work.txt"), "committed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);

    // `branch` added: the old branch is harvested, the clone re-created
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "feature/issue-12", "feature/issue-12")
        .unwrap();
    assert_eq!(head(), "feature/issue-12");
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]).trim(), tip);
    assert!(!paths.workspace.join("work.txt").exists());
    assert_eq!(
        git(&crew.repo, &["rev-parse", "refs/heads/balerix/f/c/a"]),
        sha,
        "N-4: the old branch lives on in the cache"
    );
    assert_no_auto_gc(&crew);

    // `branch` removed: back on the per-agent branch, seeded from the cache
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(head(), "balerix/f/c/a");
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]), sha);
    assert!(paths.workspace.join("work.txt").exists());
    assert_eq!(
        git(&crew.repo, &["rev-parse", "refs/heads/feature/issue-12"]).trim(),
        tip,
        "the branch the clone left is harvested too"
    );
}

/// F1: a dirty clone on the old branch fails the materialize step, naming
/// both branches, and is left exactly as it was.
#[test]
fn a_changed_branch_on_a_dirty_clone_fails_and_keeps_the_tree() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-rebranch-dirty");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    push_branch(&root, "feature/issue-12");
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    std::fs::write(paths.workspace.join("README"), "edited\n").unwrap();

    let e = ws
        .ensure_clone("f/c/a", &crew, &paths, &repo, "feature/issue-12", "feature/issue-12")
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("f/c/a: "), "{e}");
    assert!(e.contains("\"balerix/f/c/a\""), "{e}");
    assert!(e.contains("\"feature/issue-12\""), "{e}");
    assert!(e.contains("local changes"), "{e}");
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "balerix/f/c/a"
    );
    assert_eq!(
        std::fs::read_to_string(paths.workspace.join("README")).unwrap(),
        "edited\n"
    );
    assert!(
        git(&crew.repo, &["branch", "--list", "balerix/f/c/a"]).trim().is_empty(),
        "nothing was harvested from a clone that stays"
    );
}

/// Review focus 1: a `branch` the remote does not have fails the
/// materialize step with git's message, and the half-made clone is
/// removed — a `--no-checkout` clone left behind would read as an
/// existing, dirty clone on the next pass.
#[test]
fn a_missing_remote_branch_fails_the_clone_and_leaves_nothing() {
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
        .ensure_clone("f/c/pr", &crew, &paths, &repo, "nope", "nope")
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("f/c/pr: git checkout:"), "{e}");
    assert!(e.contains("origin/nope"), "{e}");
    assert!(!paths.workspace.exists(), "the half-made clone is gone");
    assert!(!paths.branch_marker().exists());
    assert!(
        git(&crew.repo, &["branch", "--list", "nope"]).trim().is_empty(),
        "no branch was created in the cache"
    );
    // the next pass fails the same way, not with "local changes"
    let again = ws
        .ensure_clone("f/c/pr", &crew, &paths, &repo, "nope", "nope")
        .unwrap_err()
        .to_string();
    assert!(again.starts_with("f/c/pr: git checkout:"), "{again}");
}

/// #60 over the clone: a restart re-materializes with the *same* setting.
/// An agent that checked out a branch of its own is neither moved back
/// nor failed for it, whether its tree is clean or dirty.
#[test]
fn an_agent_switching_branches_itself_is_left_alone_on_an_unchanged_setting() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-self-switch");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    let head = || {
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"])
            .trim()
            .to_string()
    };

    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    git(&paths.workspace, &["checkout", "-q", "-b", "my-fix"]);

    // clean tree: not moved back
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(head(), "my-fix");

    // dirty tree: not failed, and the change is kept
    std::fs::write(paths.workspace.join("README"), "edited\n").unwrap();
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(head(), "my-fix");
    assert_eq!(
        std::fs::read_to_string(paths.workspace.join("README")).unwrap(),
        "edited\n"
    );
}

/// A clone from before the marker existed is judged by its HEAD once
/// and the marker is written then; from there on HEAD no longer counts.
#[test]
fn a_clone_without_a_marker_is_judged_by_head_once_then_recorded() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-no-marker");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    let head = || {
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"])
            .trim()
            .to_string()
    };
    let ensure = |branch: &str| ws.ensure_clone("f/c/a", &crew, &paths, &repo, branch, "main");

    ensure("balerix/f/c/a").unwrap();
    std::fs::remove_file(paths.branch_marker()).unwrap();
    git(&paths.workspace, &["checkout", "-q", "-b", "my-fix"]);

    // no marker: HEAD decides, the clone is re-created, the marker appears
    ensure("balerix/f/c/a").unwrap();
    assert_eq!(head(), "balerix/f/c/a");
    assert_eq!(
        std::fs::read_to_string(paths.branch_marker()).unwrap(),
        "balerix/f/c/a"
    );
    assert!(
        git(&crew.repo, &["rev-parse", "--verify", "refs/heads/my-fix"]).trim().len() == 40,
        "the branch HEAD was on is what got harvested"
    );

    // with the marker: the same self-switch is left alone
    git(&paths.workspace, &["checkout", "-q", "-b", "my-fix"]);
    ensure("balerix/f/c/a").unwrap();
    assert_eq!(head(), "my-fix");
}

/// The agent checked out the very branch the operator then configures:
/// nothing to move, even with local changes, and the record follows.
#[test]
fn a_changed_branch_the_clone_already_sits_on_is_recorded_without_a_move() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-already-there");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    push_branch(&root, "feature/issue-12");
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    let head = || {
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"])
            .trim()
            .to_string()
    };
    let ensure = |branch: &str, start_ref: &str| {
        ws.ensure_clone("f/c/a", &crew, &paths, &repo, branch, start_ref)
    };

    ensure("balerix/f/c/a", "main").unwrap();
    git(&paths.workspace, &["checkout", "-q", "-b", "feature/issue-12"]);
    std::fs::write(paths.workspace.join("README"), "edited\n").unwrap();

    ensure("feature/issue-12", "feature/issue-12").unwrap();
    assert_eq!(head(), "feature/issue-12");
    assert_eq!(
        std::fs::read_to_string(paths.workspace.join("README")).unwrap(),
        "edited\n"
    );
    assert_eq!(
        std::fs::read_to_string(paths.branch_marker()).unwrap(),
        "feature/issue-12"
    );

    // recorded: a later self-switch is left alone under this setting too
    git(&paths.workspace, &["stash", "-q"]);
    git(&paths.workspace, &["checkout", "-q", "-b", "my-fix"]);
    ensure("feature/issue-12", "feature/issue-12").unwrap();
    assert_eq!(head(), "my-fix");
}

/// N-7 (#63): two agents on one `branch` both materialize; with private
/// clones there is no checkout to collide on.
#[test]
fn two_agents_on_one_branch_both_materialize() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-shared-branch");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let tip = push_branch(&root, "feature/issue-12");
    let crew = layout.crew(&"f/c".parse().unwrap());
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    for name in ["a", "b"] {
        let id: balerix_core::AgentId = format!("f/c/{name}").parse().unwrap();
        let paths = layout.agent(&id);
        ws.ensure_clone(&id.to_string(), &crew, &paths, &repo, "feature/issue-12", "feature/issue-12")
            .unwrap();
        assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]).trim(), tip, "{id}");
        assert!(paths.workspace.join("FEATURE").exists(), "{id}");
    }
}

/// N-6: a workspace whose `.git` is a file is a 0.1.x worktree. Refused
/// with the remedy; nothing touched.
#[test]
fn a_worktree_from_0_1_is_refused_with_the_purge_message() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-0-1");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    // what 0.1.x's ensure_worktree made
    git(&crew.repo, &["fetch", "-q", "origin"]);
    git(
        &crew.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "balerix/f/c/a",
            &paths.workspace.display().to_string(),
            "origin/main",
        ],
    );
    assert!(paths.workspace.join(".git").is_file());

    let e = ws
        .ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("f/c/a: "), "{e}");
    assert!(e.contains("created by balerix 0.1 as a worktree"), "{e}");
    assert!(e.contains("balerix down f --purge"), "{e}");
    assert!(paths.workspace.join(".git").is_file(), "left as it was");
    assert!(!paths.branch_marker().exists());
}

/// A `workspace/` with no `.git` at all — a clone that crashed half-way —
/// holds nothing balerix values; it is replaced rather than failing
/// `git clone` (`already exists and is not an empty directory`) on every
/// pass until a purge.
#[test]
fn a_crashed_clone_directory_without_git_is_replaced() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-stray");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    std::fs::create_dir_all(&paths.workspace).unwrap();
    std::fs::write(paths.workspace.join("stray.txt"), "not a clone\n").unwrap();

    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert!(paths.workspace.join(".git").is_dir());
    assert!(paths.workspace.join("README").exists());
    assert!(!paths.workspace.join("stray.txt").exists());
}

/// Review focus 5 (#62, first bullet): the clone step's `status` on a
/// branch change runs in an agent-writable repository; config the agent
/// wrote there must not run a program as the daemon.
#[test]
fn the_clone_step_runs_no_program_from_the_clone_config() {
    use std::os::unix::fs::PermissionsExt;

    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-hardened");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    push_branch(&root, "feature/issue-12");
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();

    let ran = root.join("fsmonitor-ran");
    let hook = root.join("fsmonitor.sh");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\necho\n", ran.display())).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(&paths.workspace, &["config", "core.fsmonitor", &hook.display().to_string()]);
    // the fixture bites: a plain status runs it
    git(&paths.workspace, &["status", "--porcelain"]);
    assert!(ran.exists(), "the fixture's fsmonitor hook must run under plain git");
    std::fs::remove_file(&ran).unwrap();

    // a branch change on a clean clone: `symbolic-ref` and `status` run
    // in the clone, then it is harvested and re-created
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "feature/issue-12", "feature/issue-12")
        .unwrap();
    assert!(!ran.exists(), "core.fsmonitor from the clone's config ran under the daemon");
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "feature/issue-12"
    );
}
```

- [ ] **Step 2: Update the `inspect_it` fixture**

In `crates/balerix-runtime/tests/inspect_it.rs`, replace the `ws.ensure_worktree(...)` call (lines 89–97) with:

```rust
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
```

Replace the final block's `std::fs::remove_file(w.join(".git")).unwrap();` with `std::fs::remove_dir_all(w.join(".git")).unwrap();` and its comment's "with its `.git` file deleted — the agent owns the worktree" with "with its `.git` directory deleted — the agent owns the clone". In the `extensions.worktreeConfig` comment, `<gitdir>/worktrees/<id>/config.worktree` becomes `.git/config.worktree`; "the worktree's .git file is refused before any I/O" becomes "the clone's .git is refused before any I/O".

- [ ] **Step 3: Run the two suites to see them fail**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --test workspace_it --test inspect_it`
Expected: FAIL to compile, `no method named ensure_clone`.

- [ ] **Step 4: Implement `ensure_clone`, `harvest` and the gc pin**

In `crates/balerix-runtime/src/workspace.rs`:

Replace `ensure_repo`'s doc and the tail of its body (after the clone call) so it reads:

```rust
    /// The crew's object cache (Spec N §3): a `--no-checkout` clone, made
    /// when absent, with `gc.auto=0` so that git never gcs it on its own —
    /// a clone borrowing objects from it is only safe while the cache
    /// never loses one. A cache that exists is left alone, so a
    /// steady-state pass costs no git call (Phase 3 spec §6.1);
    /// `ensure_clone` fetches when it actually needs `origin/<ref>`.
    pub fn ensure_repo(
        &self,
        id: &str,
        crew: &CrewPaths,
        repo: &RepoRef,
        git_ref: &str,
    ) -> Result<(), MaterializeError> {
        let _ = git_ref;
        if crew.repo.join(".git").is_dir() {
            return Ok(());
        }
        std::fs::create_dir_all(&crew.root).map_err(|e| MaterializeError::Io {
            id: id.to_string(),
            path: crew.root.clone(),
            message: e.to_string(),
        })?;
        let cache = crew.repo.display().to_string();
        self.git(
            id,
            crew,
            &["clone", "--quiet", "--no-checkout", &repo.clone_url(), &cache],
        )?;
        self.git(id, crew, &["-C", &cache, "config", "gc.auto", "0"])?;
        Ok(())
    }
```

Leave `is_registered` and `remove_worktree` alone (Task 4 deletes them). Replace `ensure_worktree` (doc comment through its closing brace) and `record_branch` with:

```rust
    /// One git call inside the agent's clone, hardened by
    /// `harden_agent_git` because the clone is agent-writable. `accepted`
    /// are the exit codes that count as success (0 included).
    fn agent_git(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        args: &[&str],
        accepted: &[i32],
    ) -> Result<String, MaterializeError> {
        let cmd = harden_agent_git(
            Cmd::new(&self.tools.git).log(&crew.root.join("logs").join("git.log")),
            crew,
            &agent.root,
        )
        .args(["-C".to_string(), agent.workspace.display().to_string()])
        .args(args.iter().copied());
        cmd.run_with_exit_codes(accepted)
            .map(|o| o.stdout)
            .map_err(|f| MaterializeError::Tool {
                id: id.to_string(),
                tool: f.tool,
                subcommand: f.subcommand,
                args: f.args,
                stderr: f.stderr,
            })
    }

    /// The agent's private clone on `branch` (Spec N §4): a clone of
    /// `repo` made with `--reference` to the crew cache, so objects the
    /// cache holds are neither transferred nor duplicated.
    ///
    /// An existing clone (`workspace/.git` a directory) is judged by
    /// `decide_clone`, Spec L §12's marker rule unchanged: a matching
    /// marker reuses the clone whatever its HEAD (#60); a clone without a
    /// marker is judged by HEAD once; a detached HEAD is reused as is; a
    /// changed `branch` re-creates a clean clone after harvesting the old
    /// branch into the cache (§5), and fails a dirty one naming both
    /// branches. Every git call on an existing clone is `agent_git` (#62).
    ///
    /// A `workspace/.git` *file* is a worktree from balerix 0.1, refused
    /// with the remedy (N-6). A `workspace/` with no `.git` at all (a
    /// clone that crashed half-way) is replaced.
    ///
    /// A new clone starts with a fetch in the cache — the one moment
    /// `origin/<start_ref>` must be current, and what makes the clone
    /// cheap — then seeds `branch` from the cache's harvested copy when
    /// there is one, else creates it from `origin/<start_ref>`. A seeded
    /// branch tracks `origin/<branch>` when the remote has it, as a branch
    /// created from it would. Whatever fails after `git clone` removes the
    /// half-made clone: a `--no-checkout` clone left behind would be
    /// judged an existing, dirty clone on the next pass.
    pub fn ensure_clone(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        repo: &RepoRef,
        branch: &str,
        start_ref: &str,
    ) -> Result<(), MaterializeError> {
        let dot_git = agent.workspace.join(".git");
        if dot_git.is_dir() {
            let marker = std::fs::read_to_string(agent.branch_marker()).ok();
            // Err is a detached HEAD (`ref HEAD is not a symbolic ref`),
            // reused as is: its commits may be on no branch.
            let head = self
                .agent_git(id, crew, agent, &["symbolic-ref", "--short", "HEAD"], &[0])
                .ok();
            let decision = decide_clone(marker.as_deref(), head.as_deref(), branch, || {
                self.agent_git(id, crew, agent, &["status", "--porcelain"], &[0])
                    .map(|s| !s.trim().is_empty())
            })?;
            match decision {
                CloneDecision::Reuse => return Ok(()),
                CloneDecision::Record => return Self::record_branch(id, agent, branch),
                CloneDecision::Dirty { old } => {
                    return Err(MaterializeError::Invalid {
                        id: id.to_string(),
                        message: format!(
                            "the clone was created on branch {old:?} but the agent's \
                             branch is {branch:?}, and the clone has local changes; \
                             commit or discard them in {} first",
                            agent.workspace.display()
                        ),
                    });
                }
                CloneDecision::Recreate { old } => {
                    self.harvest(id, crew, agent, &old)?;
                    remove_tree(id, &agent.workspace)?;
                }
            }
        } else if dot_git.exists() {
            // `id` is `<fleet>/<crew>/<agent>`
            let fleet = id.split('/').next().unwrap_or(id);
            return Err(MaterializeError::Invalid {
                id: id.to_string(),
                message: format!(
                    "{}: created by balerix 0.1 as a worktree; run `balerix down {fleet} \
                     --purge` and `up` again (push unpushed work first)",
                    agent.workspace.display()
                ),
            });
        } else {
            remove_tree(id, &agent.workspace)?;
        }
        if let Err(e) = self.create_clone(id, crew, agent, repo, branch, start_ref) {
            let _ = std::fs::remove_dir_all(&agent.workspace);
            return Err(e);
        }
        Self::record_branch(id, agent, branch)
    }

    /// Spec N §4 step 3. These calls run in a clone the agent has never
    /// touched, so they need no hardening; they go through `git` for the
    /// credential helper the clone needs.
    fn create_clone(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        repo: &RepoRef,
        branch: &str,
        start_ref: &str,
    ) -> Result<(), MaterializeError> {
        let cache = crew.repo.display().to_string();
        let ws = agent.workspace.display().to_string();
        self.git(
            id,
            crew,
            &["-C", &cache, "fetch", "--quiet", "--no-auto-gc", "origin"],
        )?;
        self.git(
            id,
            crew,
            &[
                "clone",
                "--quiet",
                "--no-checkout",
                "--reference",
                &cache,
                &repo.clone_url(),
                &ws,
            ],
        )?;
        let refname = format!("refs/heads/{branch}");
        let harvested = self
            .git(
                id,
                crew,
                &["-C", &cache, "rev-parse", "--verify", "--quiet", &refname],
            )
            .is_ok();
        if harvested {
            self.git(
                id,
                crew,
                &[
                    "-C",
                    &ws,
                    "fetch",
                    "--quiet",
                    "--no-auto-gc",
                    &cache,
                    &format!("{refname}:{refname}"),
                ],
            )?;
            self.git(id, crew, &["-C", &ws, "checkout", "--quiet", branch])?;
            let remote = format!("refs/remotes/origin/{branch}");
            if self
                .git(id, crew, &["-C", &ws, "rev-parse", "--verify", "--quiet", &remote])
                .is_ok()
            {
                self.git(
                    id,
                    crew,
                    &[
                        "-C",
                        &ws,
                        "branch",
                        "--quiet",
                        &format!("--set-upstream-to=origin/{branch}"),
                        branch,
                    ],
                )?;
            }
        } else {
            self.git(
                id,
                crew,
                &[
                    "-C",
                    &ws,
                    "checkout",
                    "--quiet",
                    "-b",
                    branch,
                    &format!("origin/{start_ref}"),
                ],
            )?;
        }
        Ok(())
    }

    /// Spec N §5: the clone's `refs/heads/<branch>` into the cache, by a
    /// fetch run *in the cache*. A push run in the clone would honour the
    /// clone's config (an `url.<x>.insteadOf` there could aim it at
    /// another crew's cache); `upload-pack` in the clone takes no
    /// repo-local hook or program config, and the only write is into the
    /// daemon-owned cache. The `+` is intended: the clone was seeded from
    /// the cache's copy, so the clone's is the newer state even after a
    /// rebase. A branch the clone does not have (deleted by the agent) is
    /// skipped.
    fn harvest(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        branch: &str,
    ) -> Result<(), MaterializeError> {
        let refname = format!("refs/heads/{branch}");
        let present = self.agent_git(
            id,
            crew,
            agent,
            &["rev-parse", "--verify", "--quiet", &refname],
            &[0, 1],
        )?;
        if present.trim().is_empty() {
            return Ok(());
        }
        self.git(
            id,
            crew,
            &[
                "-C",
                &crew.repo.display().to_string(),
                "fetch",
                "--quiet",
                "--no-auto-gc",
                &agent.workspace.display().to_string(),
                &format!("+{refname}:{refname}"),
            ],
        )
        .map(|_| ())
    }

    fn record_branch(id: &str, agent: &AgentPaths, branch: &str) -> Result<(), MaterializeError> {
        let marker = agent.branch_marker();
        write_atomic(&marker, branch.as_bytes(), 0o644).map_err(|e| MaterializeError::Io {
            id: id.to_string(),
            path: marker,
            message: e.to_string(),
        })
    }
```

Add, as a free function next to `decide_clone`:

```rust
/// `remove_dir_all` that treats a missing path as done. Not `Runtime::rm_rf`:
/// that one waits out nono's ledger writes, and no process writes a clone
/// while the daemon materializes or removes it.
fn remove_tree(id: &str, path: &Path) -> Result<(), MaterializeError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(MaterializeError::Io {
            id: id.to_string(),
            path: path.to_path_buf(),
            message: e.to_string(),
        }),
    }
}
```

In `crates/balerix-runtime/src/materializer.rs`, `materialize`, replace the `ensure_worktree` call and its comment with:

```rust
        // Spec N §4: a private clone on the agent's branch. Spec L §6: with
        // `branch` set, that branch is the remote branch itself, created
        // from `origin/<branch>`; without it the per-agent branch starts
        // from the crew's ref.
        self.workspace(&agent.id.fleet, &agent.git).ensure_clone(
            &id,
            &crew,
            &paths,
            &agent.repo,
            &agent.branch(),
            agent.start_ref(),
        )?;
```

- [ ] **Step 5: Run the two suites**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --test workspace_it --test inspect_it --test materialize_it`
Expected: PASS. (`materialize_it`'s `worktree list` assertion still passes — vacuously, no worktree is registered; Task 4 replaces it.) If `a_private_clone_borrows_from_the_cache_and_pushes_to_origin` fails on the `alternates` comparison, print both sides: git writes the canonical absolute path of the reference's `objects/`, and `target/tmp` may sit behind a symlink — the test canonicalizes `cache_objects()` for that reason.

- [ ] **Step 6: Gate and commit**

Run: `mise run check && mise run test-it`
Expected: PASS.

```bash
git add crates/balerix-runtime/src/workspace.rs crates/balerix-runtime/src/materializer.rs crates/balerix-runtime/tests/workspace_it.rs crates/balerix-runtime/tests/inspect_it.rs
git commit -m "feat(runtime): a private clone per agent, seeded from the crew's object cache (Spec N §4)"
```

---

### Task 4: `harvest_and_remove` — the branch survives the clone

**Files:**
- Modify: `crates/balerix-runtime/src/workspace.rs` (`remove_worktree` → `harvest_and_remove`, `assigned_branch`)
- Modify: `crates/balerix-runtime/src/materializer.rs:356-397` (`remove_agent`, `remove_crew`)
- Modify: `crates/balerix-runtime/tests/workspace_it.rs` (four tests added)
- Modify: `crates/balerix-runtime/tests/materialize_it.rs:214-239`

**Interfaces:**
- Consumes: `harvest`, `remove_tree`, `agent_git` (Task 3).
- Produces: `Workspace::harvest_and_remove(&self, id: &str, crew: &CrewPaths, agent: &AgentPaths) -> Result<(), MaterializeError>` — harvests, then deletes `workspace/`; the caller still deletes the agent root.

- [ ] **Step 1: Write the failing removal tests**

Append to `crates/balerix-runtime/tests/workspace_it.rs`:

```rust
/// N-4: an unpushed commit survives `remove_agent` and the next
/// materialize, seeded from the cache — the exact commit — and a rebase
/// after the seed is harvested over the cache's older copy (the `+`).
#[test]
fn an_unpushed_commit_survives_removal_and_seeds_the_next_clone() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-harvest");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    std::fs::write(paths.workspace.join("work.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);
    // a second local branch, and an ignored file: both go with the clone
    git(&paths.workspace, &["branch", "scratch"]);
    std::fs::write(paths.workspace.join(".gitignore"), "ignored\n").unwrap();
    std::fs::write(paths.workspace.join("ignored"), "x\n").unwrap();

    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap();
    assert!(!paths.workspace.exists());
    assert_eq!(git(&crew.repo, &["rev-parse", "refs/heads/balerix/f/c/a"]), sha);
    assert!(
        git(&crew.repo, &["branch", "--list", "scratch"]).trim().is_empty(),
        "only the assigned branch is harvested"
    );
    assert_no_auto_gc(&crew);

    // the marker survives in the agent root (remove_agent deletes that);
    // a re-created clone is seeded from the cache
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]), sha, "the exact commit");
    assert!(paths.workspace.join("work.txt").exists());
    assert!(!paths.workspace.join("ignored").exists());

    // the agent rewrites history: the cache's copy is no ancestor of the
    // clone's, and the harvest must win anyway (`+`)
    git(&paths.workspace, &["commit", "-q", "--amend", "-m", "agent work, amended"]);
    let amended = git(&paths.workspace, &["rev-parse", "HEAD"]);
    assert_ne!(amended, sha);
    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap();
    assert_eq!(
        git(&crew.repo, &["rev-parse", "refs/heads/balerix/f/c/a"]),
        amended,
        "N-5: the clone's state is the newer one, fast-forward or not"
    );
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]), amended);

    // a plain directory where a clone used to be is removed as well
    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap();
    std::fs::create_dir_all(&paths.workspace).unwrap();
    std::fs::write(paths.workspace.join("stray.txt"), "x\n").unwrap();
    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap();
    assert!(!paths.workspace.exists());
    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap(); // already gone
}

/// Review focus 3: without a marker the branch HEAD is on is harvested;
/// a detached HEAD, or a branch the agent deleted, harvests nothing and
/// the removal still succeeds.
#[test]
fn removal_harvests_head_without_a_marker_and_skips_what_is_not_there() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-harvest-edge");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let crew = layout.crew(&"f/c".parse().unwrap());
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    let make = |name: &str| {
        let id: balerix_core::AgentId = format!("f/c/{name}").parse().unwrap();
        let paths = layout.agent(&id);
        ws.ensure_clone(&id.to_string(), &crew, &paths, &repo, &format!("balerix/f/c/{name}"), "main")
            .unwrap();
        std::fs::write(paths.workspace.join("w"), name).unwrap();
        git(&paths.workspace, &["add", "."]);
        git(&paths.workspace, &["commit", "-q", "-m", name]);
        (id, paths)
    };

    // no marker (a crash between clone and marker): HEAD's branch
    let (_, a) = make("a");
    git(&a.workspace, &["checkout", "-q", "-b", "my-fix"]);
    std::fs::remove_file(a.branch_marker()).unwrap();
    let sha = git(&a.workspace, &["rev-parse", "HEAD"]);
    ws.harvest_and_remove("f/c/a", &crew, &a).unwrap();
    assert_eq!(git(&crew.repo, &["rev-parse", "refs/heads/my-fix"]), sha);
    assert!(
        git(&crew.repo, &["branch", "--list", "balerix/f/c/a"]).trim().is_empty(),
        "the marker was gone, HEAD decided"
    );

    // detached HEAD, no marker: nothing to name, nothing harvested
    let (_, b) = make("b");
    git(&b.workspace, &["checkout", "-q", "--detach"]);
    std::fs::remove_file(b.branch_marker()).unwrap();
    ws.harvest_and_remove("f/c/b", &crew, &b).unwrap();
    assert!(!b.workspace.exists());
    assert!(git(&crew.repo, &["branch", "--list", "balerix/f/c/b"]).trim().is_empty());

    // the agent deleted its assigned branch: skipped, not failed
    let (_, c) = make("c");
    git(&c.workspace, &["checkout", "-q", "-b", "elsewhere"]);
    git(&c.workspace, &["branch", "-D", "balerix/f/c/c"]);
    ws.harvest_and_remove("f/c/c", &crew, &c).unwrap();
    assert!(!c.workspace.exists());
    assert!(git(&crew.repo, &["branch", "--list", "balerix/f/c/c"]).trim().is_empty());

    // no cache at all: nothing to harvest into, the clone is still removed
    let (_, d) = make("d");
    std::fs::remove_dir_all(&crew.repo).unwrap();
    ws.harvest_and_remove("f/c/d", &crew, &d).unwrap();
    assert!(!d.workspace.exists());
}

/// A clone git cannot read fails the removal — the operator sees it
/// rather than losing work (Spec N §5).
#[test]
fn a_broken_clone_fails_the_removal_and_stays() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-harvest-broken");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    // `.git/HEAD` gone: git no longer sees a repository there
    std::fs::remove_file(paths.workspace.join(".git/HEAD")).unwrap();
    let e = ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap_err().to_string();
    assert!(e.starts_with("f/c/a: git "), "{e}");
    assert!(paths.workspace.exists(), "nothing deleted on a failed harvest");
}

/// Review focus 2: a 0.1.x fleet is refused with the purge message; after
/// `down --keep-repos` the old crew clone — no `gc.auto=0`, a stale
/// worktree registration — serves as the cache, and the branch it holds
/// seeds the new clone, so the unpushed work in it is not lost.
#[test]
fn a_worktree_from_0_1_is_refused_and_keep_repos_migrates_it() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-0-1-migrate");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    // what 0.1.x left behind: a clone without the gc pin, a worktree, an
    // unpushed commit on the worktree's branch, and no marker
    std::fs::create_dir_all(&crew.root).unwrap();
    git(&crew.root, &["clone", "-q", "--no-checkout", &repo.clone_url(), "repo"]);
    git(
        &crew.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "balerix/f/c/a",
            &paths.workspace.display().to_string(),
            "origin/main",
        ],
    );
    std::fs::write(paths.workspace.join("work.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "0.1 work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);

    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap(); // present: left alone
    let e = ws
        .ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap_err()
        .to_string();
    assert!(e.contains("created by balerix 0.1 as a worktree"), "{e}");

    // `down --keep-repos`: the worktree goes (nothing to harvest from a
    // `.git` file — its branch already lives in the old clone)
    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap();
    assert!(!paths.workspace.exists());
    // `up`: a clone seeded from the old clone's branch
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert!(paths.workspace.join(".git").is_dir());
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]), sha);
    assert!(paths.workspace.join("work.txt").exists());
    assert_no_auto_gc(&crew);
}
```

- [ ] **Step 2: Replace `materialize_it`'s worktree assertion and add the purge case**

In `crates/balerix-runtime/tests/materialize_it.rs`, replace the block from `// Exercise remove_crew's per-agent worktree-removal loop` through `rt.remove_crew(&crew, Keep::default()).unwrap(); assert!(!layout.crew(&crew).root.exists());` with:

```rust
    // Exercise remove_crew's per-agent harvest loop while the agent's
    // clone still exists: removing the agent first would empty `agents/`
    // and this loop would never run. An unpushed commit on the assigned
    // branch must reach the cache (Spec N §5).
    std::fs::write(paths.workspace.join("work.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);
    let crew_paths = layout.crew(&crew);
    rt.remove_crew(
        &crew,
        Keep {
            repos: true,
            sessions: false,
        },
    )
    .unwrap();
    assert!(!paths.root.exists());
    assert!(crew_paths.repo.exists());
    assert_eq!(
        git(&crew_paths.repo, &["rev-parse", "refs/heads/balerix/f/c/a"]),
        sha,
        "harvested before the clone went"
    );

    rt.remove_agent(&agent.id).unwrap(); // already gone: must be a no-op
    rt.remove_crew(&crew, Keep::default()).unwrap();
    assert!(!layout.crew(&crew).root.exists());
```

Then add a new test at the end of the file:

```rust
/// Review focus 4: `down --purge` over a clone git cannot read must
/// succeed — the cache goes too, so there is nothing to harvest into.
#[test]
fn a_purge_deletes_a_broken_clone_without_harvesting() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("materialize-purge-broken");
    let layout = support::layout(&root);
    std::fs::create_dir_all(&layout.config_root).unwrap();
    std::fs::write(layout.system_mise_toml(), "[tools]\n").unwrap();
    let work = root.join("up");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README"), "x").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("up.git");
    git(
        &root,
        &["clone", "-q", "--bare", &work.display().to_string(), &bare.display().to_string()],
    );
    let f = fleet(&format!("file://{}", bare.display()));
    let rt = Runtime::new(layout.clone(), tools);
    let agent = ResolvedAgent::from_fleet(&f).remove(0);
    let crew = agent.id.crew_ref();
    let creds = CredentialBundle::default();
    let (f_tools, c_tools) = (no_tools(), no_tools());
    rt.ensure_crew(
        &crew,
        &agent.repo,
        &agent.git_ref,
        &agent.git,
        &creds,
        CrewTools {
            fleet: &f_tools,
            crew: &c_tools,
        },
    )
    .unwrap();
    let paths = layout.agent(&agent.id);
    let ws = balerix_runtime::Workspace {
        tools: &rt.tools,
        gh_config_dir: None,
    };
    ws.ensure_clone(
        &agent.id.to_string(),
        &layout.crew(&crew),
        &paths,
        &agent.repo,
        &agent.branch(),
        agent.start_ref(),
    )
    .unwrap();
    std::fs::remove_file(paths.workspace.join(".git/HEAD")).unwrap();

    // keep-repos: the harvest runs and fails, and says so
    let e = rt
        .remove_crew(
            &crew,
            Keep {
                repos: true,
                sessions: false,
            },
        )
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("f/c/a: git "), "{e}");
    assert!(paths.workspace.exists());

    // purge: nothing to harvest into, everything goes
    rt.remove_crew(&crew, Keep::default()).unwrap();
    assert!(!layout.crew(&crew).root.exists());
}
```

(`fleet`, `no_tools` and `git` are the file's existing helpers; check the `fleet` helper's agent id is `f/c/a` — it is, the first test asserts `f/c/pr` beside it.)

- [ ] **Step 3: Run the suites to see them fail**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --test workspace_it --test materialize_it`
Expected: FAIL to compile, `no method named harvest_and_remove`.

- [ ] **Step 4: Implement `harvest_and_remove` and wire the two removals**

In `crates/balerix-runtime/src/workspace.rs`, delete `is_registered` and replace `remove_worktree` (doc through closing brace) with:

```rust
    /// Deletes the agent's clone after harvesting its assigned branch into
    /// the cache (Spec N §5): the branch the marker recorded — the one
    /// balerix created the clone on — or, with no marker, the branch HEAD
    /// is on. Only that branch survives: other local branches the agent
    /// created, and every file in the tree, ignored files included, go
    /// with the clone (#62). A detached HEAD with no marker, a 0.1.x
    /// worktree (`.git` a file), a bare directory or a missing cache has
    /// nothing to harvest and is deleted as it is. A failed harvest fails
    /// the removal, so the operator sees it rather than losing work;
    /// `--purge` is the way past a clone too broken to read (`remove_crew`
    /// harvests only when the cache stays).
    pub fn harvest_and_remove(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
    ) -> Result<(), MaterializeError> {
        if crew.repo.join(".git").is_dir()
            && agent.workspace.join(".git").is_dir()
            && let Some(branch) = self.assigned_branch(id, crew, agent)
        {
            self.harvest(id, crew, agent, &branch)?;
        }
        remove_tree(id, &agent.workspace)
    }

    /// The marker's branch, else HEAD's; `None` for a detached HEAD.
    fn assigned_branch(&self, id: &str, crew: &CrewPaths, agent: &AgentPaths) -> Option<String> {
        match std::fs::read_to_string(agent.branch_marker()) {
            Ok(m) if !m.trim().is_empty() => Some(m.trim().to_string()),
            _ => self
                .agent_git(id, crew, agent, &["symbolic-ref", "--short", "HEAD"], &[0])
                .ok()
                .map(|h| h.trim().to_string()),
        }
    }
```

In `crates/balerix-runtime/src/materializer.rs`, replace `remove_agent` and `remove_crew`:

```rust
    fn remove_agent(&self, agent: &AgentId) -> Result<(), MaterializeError> {
        let id = agent.to_string();
        let crew = self.layout.crew(&agent.crew_ref());
        let paths = self.layout.agent(agent);
        Workspace {
            tools: &self.tools,
            gh_config_dir: None,
        }
        .harvest_and_remove(&id, &crew, &paths)?;
        Self::rm_rf(&id, &paths.root)
    }

    fn remove_crew(&self, crew: &CrewRef, keep: Keep) -> Result<(), MaterializeError> {
        let id = crew.to_string();
        let paths = self.layout.crew(crew);
        if !keep.sessions {
            let agents_dir = paths.root.join("agents");
            // Spec N §5: with the cache staying, each clone's branch is
            // harvested into it first. Without `keep.repos` the cache goes
            // too — nothing to harvest into, and no harvest to fail a
            // `--purge` over a clone git cannot read.
            if keep.repos && let Ok(entries) = std::fs::read_dir(&agents_dir) {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let Ok(agent_id) = format!("{id}/{name}").parse::<AgentId>() else {
                        continue; // not an agent directory
                    };
                    Workspace {
                        tools: &self.tools,
                        gh_config_dir: None,
                    }
                    .harvest_and_remove(
                        &agent_id.to_string(),
                        &paths,
                        &self.layout.agent(&agent_id),
                    )?;
                }
            }
            Self::rm_rf(&id, &agents_dir)?;
        }
        if !keep.repos {
            Self::rm_rf(&id, &paths.repo)?;
        }
        if !keep.repos && !keep.sessions {
            Self::rm_rf(&id, &paths.root)?;
        }
        Ok(())
    }
```

- [ ] **Step 5: Run the suites**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --test workspace_it --test materialize_it --test inspect_it`
Expected: PASS.

- [ ] **Step 6: Gate and commit**

Run: `mise run check && mise run test-it`
Expected: PASS.

```bash
git add crates/balerix-runtime/src/workspace.rs crates/balerix-runtime/src/materializer.rs crates/balerix-runtime/tests/workspace_it.rs crates/balerix-runtime/tests/materialize_it.rs
git commit -m "feat(runtime): harvest an agent's branch into the crew cache before its clone goes (Spec N §5)"
```

---

### Task 5: Documentation, the threat model, the changelog note

**Files:**
- Modify: `docs/THREAT-MODEL.md:27`, `:41` (accepted risk removed), `:81-82`, the mitigation table rows "Sandbox mis-grants" and "Repository config running a program under the daemon", plus one new row
- Modify: `ARCHITECTURE.md:45`, `:90`, `:159-166`, `:183`, `:221-222`, `:333-337`
- Modify: `AGENTS.md` (the gotchas named below)
- Modify: `CHANGELOG.md:1-3`, `docs/RELEASING.md:50-52`
- Modify: `docs/superpowers/specs/2026-09-22-balerix-m-github-plugin-design.md:41`, `docs/superpowers/specs/2026-09-22-balerix-l-managed-fleets-design.md:221`, `docs/superpowers/specs/2026-09-25-balerix-n-private-clones-design.md` (new §12)
- Modify: `crates/balerix-api/src/branch.rs:5-6`, `crates/balerix-core/src/agent.rs:112-115`

No code changes; the gate is `mise run check` (doc comments compile) and reading each edit back against the spec's §7 and §9.

- [ ] **Step 1: The threat model**

In `docs/THREAT-MODEL.md`:

1. Adversaries, the agent bullet: "write anything into its worktree and the shared crew `.git`" → "write anything into its private clone, its `.git` included".
2. Delete the accepted-risk bullet **"Git isolation between agents of the same crew"**.
3. The `branch` bullet (line 81): replace the example `git worktree add -b <branch> <path> origin/<branch>` with `git checkout -b <branch> origin/<branch>`.
4. The "daemon runs read-only `git`" bullet (line 82): `worktree add` → "`clone --reference` and `checkout` at creation"; delete the parenthetical "(the common dir for a linked worktree)"; "the shared config" → "the clone's config" (twice); replace the final residue sentence with: "The residue is the existing trust that `clone --reference` and `checkout` at creation apply the repository's smudge filters and hooks exactly as `worktree add` did, and that the same `status --porcelain` and `symbolic-ref` run, hardened the same way (`harden_agent_git`), in a live agent's clone when its `branch` changes and at removal (Spec N §4, §5; #62)."
5. Mitigation table, row "Sandbox mis-grants": after "read-write `home/`+`workspace/`" add "; read-only on the crew's object cache `crews/<c>/repo/.git/objects`, the only path under `repo/` an agent sees".
6. Row "Repository config running a program under the daemon": "an agent can delete its worktree's `.git` file" → "an agent can delete its clone's `.git` directory"; append before the final `|`: "; the clone step's `symbolic-ref`, `status` and `rev-parse` (`Workspace::agent_git`) run the same hardening through the shared `harden_agent_git`, and `workspace_it` plants an fsmonitor script in the clone's config and asserts a branch change did not run it".
7. Add a row after it:

```
| One agent reaching a sibling's repository | refs, index, config, hooks and HEAD are per agent — a private clone made with `--reference` to the crew's cache (Spec N-1); agents share only the read-only object cache (`crews/<c>/repo/.git/objects`, granted `read`, never `allow`, N-3); the cache is written only by the daemon (`gc.auto=0`, every fetch `--no-auto-gc`, never pruned, N-2) and the daemon reads an agent's clone only by fetching from it in the cache (`upload-pack` takes no repo-local hook or program config, N-5), so nothing an agent puts in its own `.git/config` runs under the daemon or reaches another crew; `sandbox_it` (a write into the cache from inside the profile is denied, an object read succeeds), `workspace_it` (harvest by fetch) | `crates/balerix-runtime/src/sandbox.rs::balerix_grants`, `workspace.rs::{harvest, harvest_and_remove}` |
```

- [ ] **Step 2: `ARCHITECTURE.md`**

1. Line 45: "`inspect.rs` reads a worktree for the plugin host's workspace routes." → "`inspect.rs` reads an agent's clone for the plugin host's workspace routes."
2. Line 90: "clone/worktree →" → "cache fetch + private clone (`--reference` to the crew's object cache) →".
3. Lines 159–166: "reads an agent's worktree" → "reads an agent's clone"; "`git` in the worktree" → "`git` in the clone"; "The diff is the worktree against" → "The diff is the clone against"; "a cheap fingerprint of the worktree" → "of the clone".
4. Line 183: "its worktree branch and start point" → "its clone's branch and start point".
5. Replace the decision **"Worktree branches are reused, never reset."** with:

```
- **A private clone per agent, over a shared object cache (Spec N).** An
  agent's `workspace/` is a full `git clone --reference crews/<c>/repo`,
  so refs, index, config, hooks and HEAD are its own and the sandbox
  grants nothing under `repo/` but `.git/objects`, read-only. The cache is
  daemon-written and append-only (`gc.auto=0`, every fetch `--no-auto-gc`,
  never pruned): a borrower is only safe while the reference never loses
  an object. Branches are reused, never reset — `-B … origin/<ref>` would
  drop unpushed agent commits on every re-`up` after `down --keep-repos`.
- **Harvest is a fetch from the cache, not a push from the clone.** Before
  a clone is deleted its assigned branch (the `.branch` marker's) is
  fetched into the cache, `+`-forced, by a git run *in the cache*; a push
  run in the clone would honour the clone's own config, and an
  `url.<x>.insteadOf` there could aim the daemon's push at another crew's
  cache. A new clone for a branch the cache holds seeds from it, so
  unpushed work survives `down --keep-repos` and agent removal. Only the
  assigned branch is harvested.
```

6. Decision **"Git is run by the daemon, never granted to a plugin."**: delete "would need the crew repo too (a worktree's `.git` is a file pointing there) and".

- [ ] **Step 3: `AGENTS.md` gotchas**

1. The `Workspace::git` gotcha: "scrubs `GIT_DIR`…from every git call via `Cmd::env_remove`" → "(`scrub_git_env`) scrubs … from every git call; `harden_agent_git` adds the rest of the hardening for a call in an agent's clone".
2. The workspace-git-calls gotcha (`inspect.rs`): "the crew's `.git/config` is agent-writable" → "the clone's `.git/config` is agent-writable"; "deleting the worktree's `.git` file makes git refuse" → "deleting the clone's `.git` directory makes git refuse".
3. The workspace `diff` refusal gotcha: "when the crew's `.git/config` declares" → "when the clone's `.git/config` declares".
4. The `AgentSettings.branch` gotcha: "makes the *remote* branch the worktree branch and its start point" → "makes the *remote* branch the clone's branch and its start point".
5. Add, after the `AgentSettings.branch` gotcha:

```
- Every agent's `workspace/` is a private clone (Spec N); the crew's
  `repo/` is an object cache the daemon alone writes (`gc.auto=0`, every
  daemon fetch `--no-auto-gc`, never pruned) and agents read through
  `objects/info/alternates`. The sandbox grants `repo/.git/objects`
  read-only and nothing else under `repo/`. Before a clone is deleted
  (`remove_agent`, `down --keep-repos`, a changed `branch`) its marker's
  branch is fetched into the cache from inside the cache — never pushed
  from the clone — and a new clone for a branch the cache holds seeds
  from it. Only that branch survives: other local branches and every
  file in the tree go with the clone. Plain `down` and `--purge` delete
  the cache too and harvest nothing, so a broken clone cannot wedge a
  purge. A `workspace/.git` that is a *file* is a 0.1.x worktree and is
  refused with the purge message; `down --keep-repos` + `up` also works
  (the old crew clone becomes the cache and its branches seed the new
  clones).
```

- [ ] **Step 4: The changelog note and the release doc**

In `CHANGELOG.md`, insert after the `# Changelog` line and its blank line:

```
### Upgrading

- 0.2.0 gives every agent a private clone (Spec N). A fleet created by
  0.1.x is refused at the next `up` or daemon start with `created by
  balerix 0.1 as a worktree`: push unpushed work first, then `balerix down
  <fleet> --purge` and `up`. `balerix down <fleet> --keep-repos` and `up`
  also works — the old crew clone becomes the object cache and the
  branches it holds seed the new clones.

```

(`prepare.sh` prepends the generated 0.2.0 lists above this block, so it ends that version's section and rides into the GitHub Release notes, which `notes.sh` reads up to the next `## `. The spec's "above the generated entries" is not what the script does; "inside the section, after them" is what this achieves without touching the release scripts.)

In `docs/RELEASING.md`, after the "Bump rules follow cargo semver" paragraph, add:

```
A change that needs operator action gets a hand-written `### Upgrading`
block at the top of `CHANGELOG.md`, directly under the `# Changelog`
header, in the PR that makes the change. `prepare.sh` prepends the next
version's generated lists above it, so it ends that version's section and
ships in the GitHub Release notes.
```

- [ ] **Step 5: The three specs and two doc comments**

1. Spec M, decision M-10 rationale: "the branch survives in the crew repo and is reused on the next mention" → "the branch survives in the crew cache (harvested from the clone at removal, Spec N §5) and seeds the clone on the next mention".
2. Spec L §7 (line 221): "(`worktree add -b <branch> <path> origin/<branch>`)" → "(`checkout -b <branch> origin/<branch>`, since Spec N)".
3. `crates/balerix-api/src/branch.rs` module doc: "when it reaches `git worktree add -b <branch>` (Spec L §7)" → "when it reaches `git checkout -b <branch>` (Spec L §7, Spec N §4)".
4. `crates/balerix-core/src/agent.rs` `start_ref` doc: "The remote ref the worktree branch is created from when it does not exist locally" → "The remote ref the clone's branch is created from when the cache holds no harvested copy of it".
5. Append to the Spec N file:

```
## 12. Recorded at implementation (2026-09-25)

- `ensure_clone` takes `&AgentPaths` and `&RepoRef` beside the arguments
  `ensure_worktree` took: the clone needs the remote URL, and the marker
  and root come from the paths. `harvest_and_remove` takes `&AgentPaths`
  too and deletes `workspace/` itself; the caller still deletes the root.
- The harvested branch is the marker's; with no marker (a crash between
  clone and marker) it is the branch HEAD is on, and a detached HEAD
  harvests nothing. `remove_crew` harvests only when `keep.repos`: plain
  `down` and `--purge` delete the cache too, so there is nothing to
  harvest into and a broken clone cannot wedge a purge.
- A seeded branch is given `origin/<branch>` as its upstream when the
  remote has it, as `checkout -b <branch> origin/<branch>` gives a fresh
  one (the Spec L PR case after a removal).
- A failure after `git clone` removes the half-made clone: a
  `--no-checkout` clone left behind reads as an existing, dirty clone on
  the next pass. A `workspace/` with no `.git` is replaced.
- The 0.1.x message names the fleet: `run \`balerix down <fleet> --purge\``.
  `down --keep-repos` and `up` also migrates: the old crew clone serves
  as the cache (every daemon fetch is `--no-auto-gc` whether or not
  `gc.auto` is pinned) and the branches it holds seed the new clones;
  `workspace_it` proves it. The user-facing remedy stays `--purge`.
- The hardening is one builder, `harden_agent_git`, shared by the
  workspace reader and the clone step; `inspect.rs` lost its own copy.
- The Upgrading note sits under the `# Changelog` header and ends the
  0.2.0 section once `prepare.sh` prepends the generated lists, rather
  than above them (§9): that is what the release script does without a
  change to it, and `notes.sh` carries the block into the Release notes.
```

- [ ] **Step 6: Read each edit back, gate, commit**

Re-read spec §7 and §9 against the diff (`git diff -- '*.md' crates/balerix-api crates/balerix-core`): every item in those two sections has an edit. Then:

Run: `mise run check`
Expected: PASS.

```bash
git add docs/THREAT-MODEL.md ARCHITECTURE.md AGENTS.md CHANGELOG.md docs/RELEASING.md docs/superpowers/specs/2026-09-22-balerix-m-github-plugin-design.md docs/superpowers/specs/2026-09-22-balerix-l-managed-fleets-design.md docs/superpowers/specs/2026-09-25-balerix-n-private-clones-design.md crates/balerix-api/src/branch.rs crates/balerix-core/src/agent.rs
git commit -m "docs: the private clone, the object cache and the harvest in the threat model, architecture and changelog (Spec N §7, §9)"
```

---

### Task 6: The gates, the by-hand check and the PR

**Files:** none modified unless a gate fails.

- [ ] **Step 1: The three automated tiers**

Run, in order: `mise run check`, `mise run test-it`, `mise run e2e`.
Expected: each PASS. The e2e asserts `workspace/README` exists after `up` and that the managed fleet's clone came from the file's repo; both hold for a clone. Several `balerix-runtime` tests are known to flake under a busy host (AGENTS.md: `processes_with_arg_pair`, `reap_kills`); rerun once with `--no-fail-fast` before reading a failure as a regression.

- [ ] **Step 2: The 0.1.x refusal, by hand (spec §11.4)**

`~/balerix-playground` installs the *published* 0.1.1 binary (its `mise.toml`), not this build. With it, bring a one-agent fleet up (its `fleet.yaml`, state under its `xdg/`), stop that daemon, then start this branch's daemon (`mise x -- cargo run -p balerix -- serve …`) with the same `XDG_*` environment on the same state root, and confirm `balerix status` shows the agent `failed` with a message containing `created by balerix 0.1 as a worktree` and `balerix down <fleet> --purge`. Then `balerix down <fleet> --purge`, `balerix up`, and confirm `agents/<a>/workspace/.git` is a directory, `objects/info/alternates` names the cache's `objects/`, and the agent's `nono-profile.json` has no `repo` entry under `allow`. Repeat the second half once with `down --keep-repos` instead of `--purge`: `up` must succeed and the agent's earlier commit must be its clone's HEAD. Put both outcomes in the PR body.

- [ ] **Step 3: `verify-claude`, by hand (spec §11.3)**

Run: `mise run verify-claude` with the `claude` pinned in `mise.toml`. It needs a logged-in `claude`. The point of this run: a real Claude Code session starts in a clone whose `.git` is a directory (the trust dialog is keyed on the common git dir, which is now the workspace itself — `home.rs` still trusts both the workspace and `repo/`, which is harmless), and the review page's diff and live version still work over the clone. Record the outcome in the PR body; if the trust dialog appears, that is the finding — do not change `home.rs` without it.

- [ ] **Step 4: The PR**

Push the branch and open the PR with `gh pr create`:

- Title: `feat(runtime)!: a private clone per agent (Spec N)`
- Body: `Closes #63 (obsoleted: two agents on one branch both materialize). Closes the first and third bullets of #62 (the clone step's git calls are hardened; ignored files are documented as deleted with the clone); the other three bullets stay open.` Then a Summary of N-1…N-7 in three or four sentences, the **Breaking** paragraph (the Upgrading note verbatim), and the gate results block with the three command outputs from Step 1, the by-hand outcomes of Steps 2 and 3, and "No new dependency."

Expected: `pr-title.yml` accepts the title (Conventional Commit with `!`); CI's `check`, `test-it` and e2e jobs pass.
