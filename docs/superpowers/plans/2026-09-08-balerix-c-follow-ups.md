# Balerix Spec C — Follow-ups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the four follow-ups the Spec C branch review left open: refuse a workspace diff when the repository's config declares a clean/smudge/process filter (fail closed instead of "residue"); send a long single-line `send_text` through the tmux buffer too (the `send-keys` argv ceiling is ~16 KiB); stop stating the pending §8 paste verdict in Spec C's PC-5 cell; and make the two `/proc`-scanning tests in `balerix-runtime`'s `testing.rs` wait for the child's exec instead of racing it.

**Architecture:** Four independent tasks on `spec-c-follow-ups` (off `spec-c-workspace-review`), one commit per issue. Task 1 adds a `WorkspaceError::Filter` variant, one `git config` call at the top of `Runtime::diff`, the 500 mapping, an `inspect_it` case and the threat-model wording. Task 2 lowers the buffer path's trigger to "newline or over 4 KiB" with a `tmux_it` case, and Task 3 is a one-cell spec edit landed in the same dispatch. Task 4 touches only the two tests.

**Tech Stack:** as Spec C: Rust 1.98.1 (edition 2024), no new dependencies; real git 2.47 and tmux 3.7c for the integration tests.

**Spec:** `docs/superpowers/specs/2026-09-08-balerix-c-workspace-review-design.md` §3.2 (the git controls; §5's "planned follow-up" is Task 1), §3.3 (the paste; Task 2 extends it), §8 (the pending paste verdict; Task 3), §12 (record Tasks 1–2 there).

## Global Constraints

- Rust **1.98.1**, `edition = "2024"`. `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` pass at every commit. No `unwrap`/`expect` outside tests. No new workspace dependencies.
- The daemon runs `git` only through `inspect_git` (`-C <workspace>`, the five `GIT_*` variables scrubbed, `GIT_OPTIONAL_LOCKS=0`, `-c core.fsmonitor=false`, `-c core.hooksPath=<empty dir>`, `--no-ext-diff --no-textconv --no-color` on every `diff`); never `fetch`, never a credential helper.
- Library crates return `thiserror` errors; `WorkspaceError::Display` is what the plugin receives (except `Io`).
- Integration tests skip with a printed reason when a tool is missing; run them with `BALERIX_REQUIRE_TOOLS=1` so a skip cannot pass silently. Temp roots under `target/tmp`.
- The two `testing.rs` tests (`processes_with_arg_pair_finds_a_live_child_by_its_argv`, `reap_kills_the_daemon_of_a_dead_socket_and_removes_the_file`) fail intermittently in the parallel suite on a loaded host; if the pre-commit hook fails on only those, retry once (Task 4 fixes them).
- Commit messages: imperative subject, body explains why, trailer `Claude-Session: https://claude.ai/code/session_01QG3jF1gRcquRpDr3UFQDfP`. Commit with output redirected to a file (the hook prints thousands of lines).

---

### Task 1: Refuse a diff when the repository config declares a filter

**Files:**
- Modify: `crates/balerix-core/src/ports.rs` (`WorkspaceError`)
- Modify: `crates/balerix-server/src/api.rs` (`From<WorkspaceError>`)
- Modify: `crates/balerix-runtime/src/inspect.rs` (`diff`)
- Modify: `crates/balerix-runtime/tests/inspect_it.rs`
- Modify: `docs/THREAT-MODEL.md` (the accepted-risk bullet and the mitigation row), `AGENTS.md` (one gotcha), Spec C §12 (one bullet)

**Interfaces:**
- Produces:
  ```rust
  // balerix_core::WorkspaceError, new variant
  /// The repository's config names a clean/smudge/process filter; a diff would run it as the daemon.
  #[error("repository config sets {key}; workspace diff refused")]
  Filter { key: String },
  // api.rs: Filter → 500 with that text (like Tool)
  ```

- [ ] **Step 1: The variant and the mapping**

Add `Filter { key: String }` to `WorkspaceError` in `ports.rs` after `Tool`, with the exact `#[error(...)]` above and a doc comment. In `api.rs`'s `From<WorkspaceError>`, add `W::Filter { .. }` to the `INTERNAL_SERVER_ERROR` arm beside `W::Tool { .. }`. Extend the existing `fakes.rs` test that asserts `WorkspaceError` Display texts with:

```rust
        assert_eq!(
            WorkspaceError::Filter { key: "filter.lfs.clean".into() }.to_string(),
            "repository config sets filter.lfs.clean; workspace diff refused"
        );
```

Run: `mise x -- cargo test -p balerix-core fakes` and `mise x -- cargo build --workspace --all-targets`.

- [ ] **Step 2: The failing integration case**

In `crates/balerix-runtime/tests/inspect_it.rs`, after the fsmonitor block and before the "missing base" block, add:

```rust
    // a clean/smudge/process filter in repo config would run as the daemon
    git(w, &["config", "filter.pwn.clean", &hook.display().to_string()]);
    let e = rt.diff(&id, "origin/main").unwrap_err();
    assert_eq!(
        e,
        WorkspaceError::Filter {
            key: "filter.pwn.clean".into()
        }
    );
    assert!(!marker.exists(), "the filter ran");
    git(w, &["config", "--unset", "filter.pwn.clean"]);
    assert!(rt.diff(&id, "origin/main").is_ok(), "unset: diffs again");
```

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo test -p balerix-runtime --test inspect_it`
Expected: FAIL (`diff` succeeds with the filter set).

- [ ] **Step 3: The check**

In `inspect.rs`, add above `impl Runtime`:

```rust
/// Config keys whose value is a program git runs on `diff` (a clean
/// filter through `.gitattributes`) or on checkout; `--no-ext-diff` and
/// `--no-textconv` do not cover them, so a diff is refused instead.
const FILTER_KEYS: &str = r"^filter\..*\.(clean|smudge|process)$";
```

and in `diff`, right after `let git = |args, ok| …;` and before `rev-parse`:

```rust
        let filters = git(
            &["config", "--local", "--includes", "--name-only", "--get-regexp", FILTER_KEYS],
            &[0, 1],
        )?;
        if let Some(key) = filters.lines().next().map(str::trim).filter(|k| !k.is_empty()) {
            return Err(WorkspaceError::Filter { key: key.to_string() });
        }
```

(`git config --get-regexp` exits 1 when nothing matches; `--local` reads the worktree's shared `.git/config`, the file an agent can write, plus its `include.path` files.)

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo test -p balerix-runtime --test inspect_it`, `mise x -- cargo test -p balerix-runtime --lib inspect`, clippy for runtime, core, server.
Expected: PASS.

- [ ] **Step 4: The docs**

`docs/THREAT-MODEL.md`: in the accepted-risk bullet, replace the sentence from "The residue is the existing trust …" to the end of the bullet with:

```
A clean/smudge/process filter declared in `.git/config` (or a file it includes) would run as the daemon user on `diff`, so `diff` first runs `git config --local --includes --get-regexp '^filter\..*\.(clean|smudge|process)$'` and refuses with `repository config sets <key>; workspace diff refused` when anything matches (`WorkspaceError::Filter`). The residue is the existing trust that `worktree add` applies the repository's smudge filters and hooks when a worktree is created, which predates this spec.
```

In the mitigation row "Repository config running a program under the daemon", append to the mitigation cell: "; `diff` refuses when the config declares a `filter.*.{clean,smudge,process}` (`inspect_it` sets one and asserts the refusal and that it never ran)".

`AGENTS.md`: after the gotcha that starts "Workspace git calls (`balerix-runtime/src/inspect.rs`)", add:

```
- A workspace `diff` refuses with `repository config sets filter.<x>.<clean|smudge|process>; workspace diff refused` when the crew's `.git/config` declares a filter — the fix is to unset it (an agent can write that file). `read_file`/`list_dir` run no git and are unaffected.
```

Spec C §12: append the bullet "**A diff fails closed on a declared filter** (§5 called it the planned follow-up): `WorkspaceError::Filter`, 500 `repository config sets <key>; workspace diff refused`."

- [ ] **Step 5: Commit**

Subject: `Refuse a workspace diff when the repository config declares a filter`. Body: why (`--no-ext-diff`/`--no-textconv` do not cover clean filters; the config is agent-writable; fail closed rather than accept the residue). Trailer as in Global Constraints.

---

### Task 2: Long single-line `send_text` goes through the buffer too

**Files:**
- Modify: `crates/balerix-runtime/src/tmux.rs` (`send_text` and its doc comment)
- Modify: `crates/balerix-runtime/tests/tmux_it.rs` (one test)
- Modify: `AGENTS.md` (the paste gotcha), Spec C §12 (one bullet)

**Interfaces:** `AgentRunner::send_text` unchanged in signature. Produces `pub(crate) const SEND_KEYS_LIMIT: usize = 4096;` in `tmux.rs`: a text with a newline **or** longer than this goes through `load-buffer -` + `paste-buffer -p -d`; shorter single lines keep `send-keys -l`.

- [ ] **Step 1: The failing test**

Append to `tmux_it.rs` (same scaffolding as `send_text_of_seventy_kilobytes_arrives_whole`; a fresh socket `balerix-test-longline-<pid>`):

```rust
/// A single line past `send-keys`' argv ceiling (~16 KiB) is pasted like a
/// multi-line text; a short single line still goes through `send-keys -l`.
#[test]
fn a_long_single_line_send_text_arrives_whole() {
    // … scaffolding …
    let long: String = (0..20_000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
    assert!(long.len() > 16 << 10 && !long.contains('\n'));
    r.send_text(&id, "short", true).unwrap();
    r.send_text(&id, &long, true).unwrap();
    wait_for(|| std::fs::read_to_string(&stdin_log).is_ok_and(|s| s.len() >= long.len() + 7));
    let got = std::fs::read_to_string(&stdin_log).unwrap();
    assert_eq!(got, format!("short\n{long}\n"), "len {}", got.len());
    // no buffer left behind
    let buffers = std::process::Command::new(&tools.tmux).args(["-L", &socket, "list-buffers"]).output().unwrap();
    assert!(!String::from_utf8_lossy(&buffers.stdout).contains("balerix-send-"));
    r.stop_crew(&crew).unwrap();
}
```

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo test -p balerix-runtime --test tmux_it a_long_single_line`
Expected: FAIL — `send-keys` answers `command too long`.

- [ ] **Step 2: The implementation**

In `tmux.rs`, beside `SEND_SEQ`:

```rust
/// Above this many bytes a single line is pasted through a buffer like a
/// multi-line text: tmux refuses a command whose argv exceeds ~16 KiB
/// (`command too long`), and `send-keys -l` carries the text as argv.
pub(crate) const SEND_KEYS_LIMIT: usize = 4096;
```

and change the branch condition to `if text.contains('\n') || text.len() > SEND_KEYS_LIMIT {`. Update the doc comment's first sentence to "One short line goes through `send-keys -l`; text with a newline, or longer than `SEND_KEYS_LIMIT`, goes through a named buffer …".

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo test -p balerix-runtime --test tmux_it` and clippy.
Expected: PASS (all tmux tests).

- [ ] **Step 3: Docs and commit**

`AGENTS.md` paste gotcha: "`send_text` with a newline, or longer than 4 KiB, goes through `load-buffer -` + `paste-buffer -p -d` …". Spec C §12: append "**A single line over 4 KiB is pasted through the buffer too**: `send-keys -l` carries the text as argv, which tmux caps at ~16 KiB."

Commit subject: `Paste a long single-line send_text through the tmux buffer`. Trailer as in Global Constraints.

---

### Task 3: Spec C PC-5 no longer states the pending paste verdict

**Files:**
- Modify: `docs/superpowers/specs/2026-09-08-balerix-c-workspace-review-design.md` (the PC-5 row, line ~27)

- [ ] **Step 1: The edit and its commit**

In the PC-5 rationale cell, replace "a bracketed paste is what a terminal does and Claude Code takes it as one message." with "a bracketed paste is what a terminal does, and Claude Code is expected to take it as one message (§8, verified by hand with `verify-claude`)."

Commit subject: `Hedge PC-5 on the paste verdict that §8 records as pending`. Trailer as in Global Constraints. (Landed by the Task 2 implementer as a second, separate commit.)

---

### Task 4: The two `/proc`-scanning tests wait for the child's exec

**Files:**
- Modify: `crates/balerix-runtime/src/testing.rs` (the `mod tests` only)

**Interfaces:** none new outside the test module. Produces, in `mod tests`:

```rust
    /// Polls `pred` every 10 ms for up to 5 s: a spawned `sh` and the
    /// `sleep` it forks each carry the parent's argv until they exec, so
    /// a `/proc` scan taken at once may see one pid too many or too few.
    fn eventually(mut pred: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if pred() { return true; }
            if std::time::Instant::now() >= deadline { return false; }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
```

- [ ] **Step 1: The change**

`processes_with_arg_pair_finds_a_live_child_by_its_argv`: replace the single `let found = …; … assert_eq!(found, vec![child.id()]);` with

```rust
        let expected = vec![child.id()];
        let settled = eventually(|| processes_with_arg_pair("--tmux-socket", &marker) == expected);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(settled, "the child (and only the child) carries the marker once it has exec'd");
```

`reap_kills_the_daemon_of_a_dead_socket_and_removes_the_file`: replace the `assert_eq!(processes_with_arg_pair("--tmux-socket", &name), vec![daemon.id()]);` before `reap_dead_sessions` with `assert!(eventually(|| processes_with_arg_pair("--tmux-socket", &name) == vec![daemon.id()]), "the daemon is visible once it has exec'd");`. If the test later asserts the daemon is gone with a bare `processes_with_arg_pair(..).is_empty()`, wrap that in `eventually` too (a killed process can linger as a zombie until waited).

Run: `for i in 1 2 3 4 5; do mise x -- cargo nextest run -p balerix-runtime --lib -E 'test(processes_with_arg_pair_finds) | test(reap_kills_the_daemon)'; done` (expect 5 green) and `mise x -- cargo test -p balerix-runtime --lib`.

- [ ] **Step 2: Commit**

Subject: `Wait for the child's exec in the /proc-scanning tests`. Body: the fork/exec window and the load it was seen under. Trailer as in Global Constraints. Also drop the Global Constraints line about retrying the hook from `AGENTS.md`? No — `AGENTS.md` never mentioned it; nothing to remove.

---

### Task 5: The e2e waits for the review's last line before reading stdin

**Files:**
- Modify: `crates/balerix/tests/e2e.rs` (`web_journey`, the `wait_file_until` on `fake-claude.stdin`)

Found while landing Task 4: `fake-claude` records the paste line by line, and the wait predicate fired on a middle line ("Please expand these notes."), so under load the file was read before "Overall:\nLooks fine." had been written and the last assertion failed.

- [ ] **Step 1: The predicate**

Change the `wait_file_until` closure to `|s| s.contains("Looks fine.")` (the message's last line; the review body's `summary` is `Looks fine.`). Keep the three substring assertions as they are. Add a one-line comment above: `// the paste is pumped line by line: wait for the last line, not a middle one`.

Run: `mise run e2e` (or `mise x -- cargo nextest run -p balerix --test e2e web_journey` after `mise run package-plugins`).

- [ ] **Step 2: Commit**

Subject: `Wait for the review's last line before reading fake-claude's stdin`. Body: the line-by-line pump and the load it was seen under. Trailer as in Global Constraints. Only `e2e.rs` and this plan file in the commit.
