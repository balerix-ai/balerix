# Stop Ends the Process Tree (#107) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When `stop_agent`, `stop_crew` or a restart returns `Ok`, no process the agent started is alive, including one detached with `setsid` or a double fork.

**Architecture:** `launch.sh` starts the agent under `balerix agent-supervise -- nono …`, a child subreaper: every orphan below it re-parents to it, and on a hangup (or the main child's own exit) it kills every descendant until it has no children, then exits. `TmuxRunner` reads the pane's pid and start time before it kills or respawns a window and polls until that process is gone, so the wrapper's exit is the "tree is empty" signal. The `AgentRunner` port does not change.

**Tech Stack:** Rust (edition 2024, `unsafe` forbidden), `rustix` 1.1.4 (`process` feature) for the subreaper flag, `kill` and `waitpid`, tokio's unix signals in the CLI, `cargo nextest` through mise, tmux 3.7c, nono 0.79.0.

**Spec:** `docs/superpowers/specs/2026-10-01-balerix-n-sandboxed-daemon-git-design.md` §13 (read §13.3 to §13.6 before any task).

## Global Constraints

- Branch `fix/stop-process-tree`. Run cargo only through mise: `mise x -- cargo …` or `mise run <task>`.
- `mise run check` passes before every commit.
- No `unsafe`. `clippy::unwrap_used` and `clippy::expect_used` warn in non-test code; integration test files start with `#![allow(clippy::unwrap_used, clippy::expect_used)]`.
- One new direct dependency only: `rustix` 1.1.4, `default-features = false`, features `std` and `process`.
- Constants, verbatim from the spec: `STOP_GRACE` = 2 s, `STOP_WAIT` = 5 s, the runner polls every 10 ms, the wrapper exits 143 when stopped before the main child exited, 128+n when a signal killed the main child.
- Error text: `agent processes still running after stop (pid <n>)`.
- The subcommand is `agent-supervise`, hidden, and takes its command after `--`.
- The `AgentRunner` port and `FakeRunner` do not change. The nono profile does not change. `render_plugin_launch` does not change.
- A subreaper is never set inside a test process: it would adopt, and the kill loop would kill, other tests' children. Tests of the wrapper run the real `balerix` binary.
- Test roots live under `target/tmp` (`support::temp_root`, or `tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))`), never `/tmp`.
- Tests that need tmux or nono skip with a printed reason when the tool is missing; `BALERIX_REQUIRE_TOOLS=1` makes the skip a failure.
- PR title: `fix(runtime): stop ends the sandbox's whole process tree and waits for it to be empty (#107)`.

## Review Focus

Each line names the task whose tests pin it.

1. The agent's command does not exist or cannot be executed: the wrapper prints `balerix agent-supervise: …`, exits non-zero and does not hang. Task 2, `a_command_that_does_not_exist_fails_with_a_message`.
2. The main child ignores the hangup, and a second signal arrives during the grace: the tree is still killed after the full grace, once. Task 2, `a_main_child_that_ignores_the_hangup_is_killed_after_the_grace`.
3. `stop` on a window whose pane has already exited, or that does not exist: `Ok` at once, nothing to wait for. Task 3, `stop_waits_for_the_pane_process_and_a_dead_pane_stops_at_once`.
4. The pane's process is a zombie, or its pid has been reused, when the runner polls: both read as gone, so a stop never waits on a process that is not the agent. Task 1, `a_zombie_and_a_reaped_pid_are_gone`.
5. A process whose command name holds spaces or `) ` (anything an agent names itself): the `/proc/<pid>/stat` parser still finds the parent pid, so the walk does not lose that subtree. Task 1, `parse_stat_survives_a_command_name_with_parentheses_and_spaces`.

## File Structure

| File | Change |
|------|--------|
| `Cargo.toml` | `rustix` in `[workspace.dependencies]` |
| `crates/balerix-runtime/Cargo.toml` | `rustix = { workspace = true }` |
| `crates/balerix-runtime/src/supervise.rs` | new: the wrapper's loop, the `/proc` walk, `ProcIdentity` |
| `crates/balerix-runtime/src/lib.rs` | `pub mod supervise;` |
| `crates/balerix/src/cli.rs`, `main.rs`, `commands/mod.rs` | the hidden `agent-supervise` subcommand |
| `crates/balerix/src/commands/supervise.rs` | new: signal handlers, exit code |
| `crates/balerix/tests/cli_supervise.rs` | new: the wrapper against real processes, then with tmux |
| `crates/balerix-core/src/ports.rs` | `RunnerError::StillRunning` |
| `crates/balerix-runtime/src/tmux.rs` | `stop_wait`, `wait_gone`, the three waits |
| `crates/balerix-runtime/tests/tmux_it.rs` | the runner's waits, no wrapper |
| `crates/balerix-runtime/src/launch.rs` | the wrapper in front of nono |
| `crates/balerix-runtime/tests/snapshots/generated_golden__payments_generated.snap` | regenerated |
| `crates/balerix-runtime/tests/sandbox_it.rs` | the signal refusal |
| `docs/THREAT-MODEL.md`, `ARCHITECTURE.md`, `AGENTS.md`, `crates/balerix-runtime/src/materializer.rs` (comments) | documents |

---

### Task 1: `supervise.rs`, the wrapper's logic

**Files:**
- Modify: `Cargo.toml` (`[workspace.dependencies]`), `crates/balerix-runtime/Cargo.toml`, `crates/balerix-runtime/src/lib.rs`
- Create: `crates/balerix-runtime/src/supervise.rs`

**Interfaces:**
- Produces:
  - `pub const STOP_GRACE: Duration` (2 s), `pub const STOPPED_STATUS: i32` (143)
  - `pub fn supervise(argv: &[OsString], stop: &Receiver<()>, grace: Duration) -> io::Result<i32>`: marks the calling process a subreaper, runs `argv`, returns the exit status the wrapper should exit with. One message on `stop` (or the sender dropping) means "stop now".
  - `pub struct ProcIdentity { pub pid: u32, .. }` with `pub fn of(pid: u32) -> Option<Self>` and `pub fn gone(&self) -> bool`; `Copy`, `Debug`, `PartialEq`.

`supervise` itself is exercised in Task 2 through the binary (see Global Constraints). This task's tests cover the pure parts and `ProcIdentity`.

- [ ] **Step 1: Add the dependency**

In `Cargo.toml`, under `[workspace.dependencies]`, in alphabetical position:

```toml
# The agent supervisor's three syscalls (Spec N amendment §13): the child
# subreaper flag, kill and waitpid, without `unsafe`.
rustix = { version = "1.1.4", default-features = false, features = ["std", "process"] }
```

In `crates/balerix-runtime/Cargo.toml`, under `[dependencies]`, after `portable-pty`:

```toml
rustix = { workspace = true }
```

In `crates/balerix-runtime/src/lib.rs`, after `pub mod sandbox;`:

```rust
pub mod supervise;
```

- [ ] **Step 2: Write the failing tests**

Create `crates/balerix-runtime/src/supervise.rs` with only the module doc and the test module:

```rust
//! The agent supervisor (Spec N amendment §13): `balerix agent-supervise`
//! runs an agent's command as a child subreaper, so every process the
//! agent detaches re-parents here and not to pid 1, and ends the whole
//! tree on stop or when the main child exits. Also the process identity
//! `TmuxRunner` polls to learn that the wrapper is gone.

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_stat_survives_a_command_name_with_parentheses_and_spaces() {
        // field 2 is the command name in parentheses and may itself hold
        // `) `; the fields after the LAST `) ` are state, ppid, …, and
        // starttime is field 22
        let text = "4242 (a) b (c) S 17 4242 4242 0 -1 4194304 1 2 3 4 5 6 7 8 20 0 1 0 987654 10 11";
        let stat = parse_stat(text).unwrap();
        assert_eq!((stat.state, stat.ppid, stat.start), ('S', 17, 987_654));
        assert!(parse_stat("").is_none());
        assert!(parse_stat("1 (x) S 1").is_none(), "too few fields");
    }

    #[test]
    fn descendants_follow_parents_and_ignore_everything_else() {
        // 10 → 11 → 12, 10 → 13; 20 → 21 is another tree; 1 is init
        let parents = [(10, 1), (11, 10), (12, 11), (13, 10), (20, 1), (21, 20)];
        let mut found = descendants_of(10, &parents);
        found.sort_unstable();
        assert_eq!(found, vec![11, 12, 13]);
        assert!(descendants_of(12, &parents).is_empty());
    }

    #[test]
    fn a_live_process_has_an_identity_and_is_not_gone() {
        let me = ProcIdentity::of(std::process::id()).unwrap();
        assert_eq!(me.pid, std::process::id());
        assert!(!me.gone());
    }

    #[test]
    fn a_zombie_and_a_reaped_pid_are_gone() {
        let mut child = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        let id = ProcIdentity::of(child.id()).unwrap();
        assert!(!id.gone());
        child.kill().unwrap();
        // not waited yet: a zombie, which has exited as far as a stop cares
        let start = std::time::Instant::now();
        while !id.gone() {
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "never a zombie");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ProcIdentity::of(child.id()).is_none(), "a zombie has no identity");
        child.wait().unwrap();
        assert!(id.gone(), "and stays gone once reaped");
    }

    #[test]
    fn a_reused_pid_is_a_different_process() {
        let me = ProcIdentity::of(std::process::id()).unwrap();
        let other = ProcIdentity { start: me.start + 1, ..me };
        assert!(other.gone(), "same pid, another start time");
    }

    #[test]
    fn exit_codes_follow_the_shell_convention() {
        assert_eq!(exit_code(Some(7), None), 7);
        assert_eq!(exit_code(None, Some(9)), 137);
        assert_eq!(exit_code(None, None), 1);
    }
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `mise x -- cargo nextest run -p balerix-runtime --lib supervise`
Expected: does not compile (`parse_stat`, `descendants_of`, `ProcIdentity`, `exit_code` not found).

- [ ] **Step 4: Write the implementation**

Insert between the module doc and the test module:

```rust
use std::ffi::OsString;
use std::io;
use std::process::Command;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use rustix::io::Errno;
use rustix::process::{
    Pid, Signal, WaitOptions, getpid, kill_process, set_child_subreaper, waitpid,
};

/// How long the tree gets to exit after the hangup before it is killed
/// (NS-9). Fixed.
pub const STOP_GRACE: Duration = Duration::from_secs(2);
/// The wrapper's exit status when it was stopped before the main child
/// exited: 128 + SIGTERM, what a shell reports for a terminated job.
pub const STOPPED_STATUS: i32 = 143;
/// How often the wrapper looks for an exited child or a stop.
const POLL: Duration = Duration::from_millis(20);
/// The pause between two passes of the kill loop.
const KILL_STEP: Duration = Duration::from_millis(2);

/// The three fields of `/proc/<pid>/stat` this module reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stat {
    pub state: char,
    pub ppid: u32,
    pub start: u64,
}

/// Field 2 is the command name in parentheses and can hold anything,
/// `) ` included, so the fixed fields are counted from the last `) `:
/// state (3), ppid (4) and starttime (22).
pub(crate) fn parse_stat(text: &str) -> Option<Stat> {
    let (_, rest) = text.rsplit_once(") ")?;
    let mut fields = rest.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    // the next field is 5; starttime is 22
    let start = fields.nth(17)?.parse().ok()?;
    Some(Stat { state, ppid, start })
}

fn read_stat(pid: u32) -> Option<Stat> {
    parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// One process, told apart from a later one that reuses its pid by its
/// start time. A zombie has no identity: it has exited, and only waits
/// for its parent to reap it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcIdentity {
    pub pid: u32,
    start: u64,
}

impl ProcIdentity {
    pub fn of(pid: u32) -> Option<Self> {
        let stat = read_stat(pid)?;
        (stat.state != 'Z').then_some(Self {
            pid,
            start: stat.start,
        })
    }

    /// The process has exited, whether or not its pid is in use again.
    pub fn gone(&self) -> bool {
        Self::of(self.pid) != Some(*self)
    }
}

/// Every pid whose chain of parents reaches `root`, from `(pid, ppid)`
/// pairs. `root` itself is not included.
pub(crate) fn descendants_of(root: u32, parents: &[(u32, u32)]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut todo = vec![root];
    while let Some(parent) = todo.pop() {
        for &(pid, ppid) in parents {
            if ppid == parent && pid != root && !out.contains(&pid) {
                out.push(pid);
                todo.push(pid);
            }
        }
    }
    out
}

/// `(pid, ppid)` of every process `/proc` shows. A process that exits
/// while this reads is skipped.
fn proc_parents() -> Vec<(u32, u32)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| Some((pid, read_stat(pid)?.ppid)))
        .collect()
}

fn raw(pid: Pid) -> u32 {
    u32::try_from(pid.as_raw_pid()).unwrap_or(0)
}

fn pid_of(pid: u32) -> Option<Pid> {
    Pid::from_raw(i32::try_from(pid).ok()?)
}

/// A shell's convention: the exit status, or 128 + the signal that killed
/// the process.
pub(crate) fn exit_code(exited: Option<i32>, signalled: Option<i32>) -> i32 {
    exited.or(signalled.map(|s| 128 + s)).unwrap_or(1)
}

/// Reaps every child that has exited, recording the main child's status.
/// `false` when this process has no children left at all: because it is
/// a subreaper, that means its whole tree is empty.
fn reap(main: u32, status: &mut Option<i32>) -> io::Result<bool> {
    loop {
        match waitpid(None, WaitOptions::NOHANG) {
            Ok(Some((pid, st))) => {
                if raw(pid) == main {
                    *status = Some(exit_code(st.exit_status(), st.terminating_signal()));
                }
            }
            Ok(None) => return Ok(true),
            Err(e) if e == Errno::CHILD => return Ok(false),
            Err(e) if e == Errno::INTR => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// SIGKILL to every descendant, reap, repeat until there are no children.
/// It does not give up: the caller of `stop` has the bound (§13.5).
fn kill_until_empty(main: u32, status: &mut Option<i32>) -> io::Result<()> {
    let me = raw(getpid());
    while reap(main, status)? {
        for pid in descendants_of(me, &proc_parents()) {
            if let Some(pid) = pid_of(pid) {
                // gone already is fine; the next pass sees what is left
                let _ = kill_process(pid, Signal::KILL);
            }
        }
        std::thread::sleep(KILL_STEP);
    }
    Ok(())
}

/// Runs `argv` under this process as a child subreaper and returns the
/// status to exit with (§13.3). One message on `stop`, or its sender
/// dropping, is a stop: hangup to the main child, `grace` for the tree to
/// empty, then the kill loop. The main child's own exit goes straight to
/// the kill loop (NS-10).
///
/// Must be the only thing its process does: every orphan on the host
/// below this process re-parents to it, and the kill loop kills them all.
pub fn supervise(argv: &[OsString], stop: &Receiver<()>, grace: Duration) -> io::Result<i32> {
    let Some((program, args)) = argv.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no command given",
        ));
    };
    set_child_subreaper(Some(getpid()))?;
    let main = Command::new(program).args(args).spawn()?.id();
    let mut status = None;
    let mut stopped = false;
    while reap(main, &mut status)? && status.is_none() {
        match stop.recv_timeout(POLL) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                stopped = true;
                break;
            }
        }
    }
    if stopped && status.is_none() {
        // not reaped yet, so the pid is still the main child's
        if let Some(pid) = pid_of(main) {
            let _ = kill_process(pid, Signal::HUP);
        }
        let deadline = Instant::now() + grace;
        while reap(main, &mut status)? && Instant::now() < deadline {
            std::thread::sleep(POLL / 2);
        }
    }
    kill_until_empty(main, &mut status)?;
    Ok(if stopped {
        STOPPED_STATUS
    } else {
        status.unwrap_or(STOPPED_STATUS)
    })
}
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `mise x -- cargo nextest run -p balerix-runtime --lib supervise`
Expected: 6 tests pass.

- [ ] **Step 6: Lint and commit**

Run: `mise run check`
Expected: passes. If `cargo deny` or the audit rejects `rustix` as a direct dependency, stop and report the message; do not add an exception.

```bash
git add Cargo.toml Cargo.lock crates/balerix-runtime/Cargo.toml crates/balerix-runtime/src/lib.rs crates/balerix-runtime/src/supervise.rs
git commit -m "feat(runtime): the agent supervisor's loop and a process's identity (#107)"
```

---

### Task 2: `balerix agent-supervise`

**Files:**
- Modify: `crates/balerix/src/cli.rs` (the `// -- internal --` block of `Command`), `crates/balerix/src/main.rs`, `crates/balerix/src/commands/mod.rs`
- Create: `crates/balerix/src/commands/supervise.rs`, `crates/balerix/tests/cli_supervise.rs`

**Interfaces:**
- Consumes: `balerix_runtime::supervise::{STOP_GRACE, supervise}` (Task 1).
- Produces: the command line `balerix agent-supervise -- <argv…>`. It exits with the main child's status, 143 when stopped by SIGHUP, SIGTERM or SIGINT, 1 with `balerix agent-supervise: <error>` on stderr when the command cannot be started, 2 (clap) with no command.

- [ ] **Step 1: Write the failing tests**

Create `crates/balerix/tests/cli_supervise.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `balerix agent-supervise` against real processes (Spec N amendment
//! §13.7). The wrapper is a child subreaper, so it is always its own
//! process here, never the test's.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use balerix_runtime::testing::pid_alive;

const BALERIX: &str = env!("CARGO_BIN_EXE_balerix");

/// A `sleep` duration no other process on the host uses: the test
/// process's pid and a number per test. Every process a test starts
/// carries it on its command line.
fn marker(n: u32) -> String {
    format!("9999.{}{n}", std::process::id())
}

/// Live (not zombie) processes whose command line contains `needle`,
/// with their command lines.
fn with_cmdline(needle: &str) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let cmd = String::from_utf8_lossy(&raw).replace('\0', " ");
        if cmd.contains(needle) && pid_alive(pid) {
            out.push((pid, cmd));
        }
    }
    out
}

/// The `sleep <marker>` processes themselves, not the shells that name
/// them in a script.
fn sleepers(marker: &str) -> Vec<u32> {
    with_cmdline(marker)
        .into_iter()
        .filter(|(_, cmd)| cmd.starts_with("sleep "))
        .map(|(pid, _)| pid)
        .collect()
}

fn wait_for(mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn supervise(script: &str) -> Child {
    Command::new(BALERIX)
        .args(["agent-supervise", "--", "/bin/sh", "-c", script])
        .stdin(Stdio::null())
        .spawn()
        .unwrap()
}

fn signal(child: &Child, sig: &str) {
    assert!(
        Command::new("kill")
            .args([sig, &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
}

/// #107: a `setsid` sleeper and a double-forked one survive the hangup,
/// so the wrapper waits out the grace and kills them.
#[test]
fn a_hangup_ends_the_detached_sleepers_after_the_grace() {
    let m = marker(1);
    let mut w = supervise(&format!(
        "setsid sleep {m} & ( setsid sh -c 'sleep {m} &' & ); exec sleep {m}"
    ));
    wait_for(|| sleepers(&m).len() == 3);
    let start = Instant::now();
    signal(&w, "-HUP");
    let status = w.wait().unwrap();
    let took = start.elapsed();
    assert!(
        with_cmdline(&m).is_empty(),
        "nothing of the agent is alive when the wrapper returns: {:?}",
        with_cmdline(&m)
    );
    assert_eq!(status.code(), Some(143));
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_secs(4),
        "the grace, then the kill: {took:?}"
    );
}

#[test]
fn a_child_that_exits_on_the_hangup_ends_the_wrapper_inside_the_grace() {
    let m = marker(2);
    let mut w = supervise(&format!("exec sleep {m}"));
    wait_for(|| sleepers(&m).len() == 1);
    let start = Instant::now();
    signal(&w, "-HUP");
    let status = w.wait().unwrap();
    assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());
    assert_eq!(status.code(), Some(143));
    assert!(with_cmdline(&m).is_empty());
}

/// NS-10: no grace when the main child exits by itself.
#[test]
fn the_main_childs_own_exit_kills_the_rest_and_its_status_is_the_wrappers() {
    let m = marker(3);
    let start = Instant::now();
    let mut w = supervise(&format!("setsid sleep {m} & sleep 0.3; exit 7"));
    let status = w.wait().unwrap();
    assert!(start.elapsed() < Duration::from_millis(1500), "{:?}", start.elapsed());
    assert_eq!(status.code(), Some(7));
    assert!(with_cmdline(&m).is_empty(), "{:?}", with_cmdline(&m));
}

#[test]
fn a_main_child_killed_by_a_signal_reports_128_plus_the_signal() {
    let mut w = supervise("kill -KILL $$");
    assert_eq!(w.wait().unwrap().code(), Some(137));
}

/// A moving target: each generation detaches the next and exits.
#[test]
fn a_chain_that_respawns_itself_is_emptied() {
    let m = marker(5);
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let chain = dir.path().join("chain.sh");
    std::fs::write(
        &chain,
        "sleep 0.01; setsid /bin/sh \"$0\" \"$1\" >/dev/null 2>&1 &\n",
    )
    .unwrap();
    let mut w = supervise(&format!(
        "setsid /bin/sh {} {m} & exec sleep {m}",
        chain.display()
    ));
    wait_for(|| sleepers(&m).len() == 1);
    std::thread::sleep(Duration::from_millis(300));
    signal(&w, "-HUP");
    assert_eq!(w.wait().unwrap().code(), Some(143));
    assert!(with_cmdline(&m).is_empty(), "{:?}", with_cmdline(&m));
    std::thread::sleep(Duration::from_millis(200));
    assert!(with_cmdline(&m).is_empty(), "no generation escaped");
}

/// The main child ignores the hangup; a second signal during the grace
/// neither shortens it nor breaks the wrapper.
#[test]
fn a_main_child_that_ignores_the_hangup_is_killed_after_the_grace() {
    let m = marker(6);
    let mut w = supervise(&format!("trap '' HUP TERM; while :; do sleep {m}; done"));
    wait_for(|| sleepers(&m).len() == 1);
    let start = Instant::now();
    signal(&w, "-HUP");
    std::thread::sleep(Duration::from_millis(200));
    signal(&w, "-TERM");
    let status = w.wait().unwrap();
    let took = start.elapsed();
    assert_eq!(status.code(), Some(143));
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_secs(4),
        "{took:?}"
    );
    assert!(with_cmdline(&m).is_empty(), "{:?}", with_cmdline(&m));
}

#[test]
fn a_command_that_does_not_exist_fails_with_a_message() {
    let out = Command::new(BALERIX)
        .args(["agent-supervise", "--", "/no/such/binary"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("balerix agent-supervise:"), "{stderr}");
}

#[test]
fn no_command_is_a_usage_error() {
    let out = Command::new(BALERIX)
        .arg("agent-supervise")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// The agent's terminal is the wrapper's: nothing is piped or buffered.
#[test]
fn the_childs_stdin_and_stdout_pass_through() {
    assert_cmd::Command::new(BALERIX)
        .args(["agent-supervise", "--", "cat"])
        .write_stdin("through\n")
        .assert()
        .success()
        .stdout("through\n");
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `mise x -- cargo nextest run -p balerix --test cli_supervise`
Expected: every test fails; clap prints `unrecognized subcommand 'agent-supervise'`.

- [ ] **Step 3: Add the subcommand**

In `crates/balerix/src/cli.rs`, in `enum Command`, after `HookRelay,`:

```rust
    /// Runs an agent's command as a child subreaper and ends its whole
    /// process tree on stop (Spec N amendment §13). `launch.sh` calls it.
    #[command(hide = true)]
    AgentSupervise {
        #[arg(last = true, required = true, value_name = "ARGV")]
        argv: Vec<std::ffi::OsString>,
    },
```

Create `crates/balerix/src/commands/supervise.rs`:

```rust
//! `balerix agent-supervise -- <argv…>` (Spec N amendment §13):
//! `launch.sh` starts every agent under it. This file owns the signals
//! and the exit code; the loop is `balerix_runtime::supervise`.

use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::mpsc;

use balerix_runtime::supervise::{STOP_GRACE, supervise};
use tokio::signal::unix::{SignalKind, signal};

pub fn agent_supervise_command(argv: &[OsString]) -> ExitCode {
    match run(argv) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("balerix agent-supervise: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(argv: &[OsString]) -> std::io::Result<i32> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    // Installed before the child exists: a hangup that arrived first
    // would otherwise kill the wrapper and leave the child unsupervised.
    let (mut hup, mut term, mut int) = {
        let _guard = rt.enter();
        (
            signal(SignalKind::hangup())?,
            signal(SignalKind::terminate())?,
            signal(SignalKind::interrupt())?,
        )
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        rt.block_on(async move {
            loop {
                tokio::select! {
                    _ = hup.recv() => {}
                    _ = term.recv() => {}
                    _ = int.recv() => {}
                }
                if tx.send(()).is_err() {
                    break;
                }
            }
        });
    });
    supervise(argv, &rx, STOP_GRACE)
}
```

In `crates/balerix/src/commands/mod.rs`, add `pub mod supervise;` in alphabetical position among the existing `pub mod` lines.

In `crates/balerix/src/main.rs`, the wrapper's exit code is the child's, so it cannot go through `run() -> Result<String>`. Replace `main` and the head of `run`:

```rust
fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Command::AgentSupervise { argv } = &cli.command {
        return commands::supervise::agent_supervise_command(argv);
    }
    match run(cli) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<String> {
    match cli.command {
```

and add, as the last arm of that `match`, after `Command::HookRelay => …`:

```rust
        Command::AgentSupervise { .. } => {
            anyhow::bail!("agent-supervise is handled in main")
        }
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `mise x -- cargo nextest run -p balerix --test cli_supervise`
Expected: 9 tests pass. The two grace tests take a little over 2 s each.

If `a_chain_that_respawns_itself_is_emptied` fails once in several runs, that is the fork race of spec §13.6 showing at this chain's speed: run it 20 times (`--stress-count 20` is not available in every nextest; use `for i in $(seq 20); do mise x -- cargo nextest run -p balerix --test cli_supervise a_chain || break; done`) and report the count rather than loosening the assertion.

- [ ] **Step 5: Lint and commit**

Run: `mise run check`
Expected: passes.

```bash
git add crates/balerix/src/cli.rs crates/balerix/src/main.rs crates/balerix/src/commands/mod.rs crates/balerix/src/commands/supervise.rs crates/balerix/tests/cli_supervise.rs
git commit -m "feat(cli): agent-supervise, a child subreaper that ends the agent's whole tree (#107)"
```

---

### Task 3: The runner waits

**Files:**
- Modify: `crates/balerix-core/src/ports.rs` (`RunnerError`), `crates/balerix-runtime/src/tmux.rs` (`TmuxRunner`, `stop_agent`, `stop_crew`, the `Running` arm of `ensure_agent`, the unit tests), `crates/balerix-runtime/src/lib.rs` (re-export)
- Test: `crates/balerix-runtime/tests/tmux_it.rs`

**Interfaces:**
- Consumes: `crate::supervise::ProcIdentity` (Task 1): `ProcIdentity::of(pid: u32) -> Option<ProcIdentity>`, `gone(&self) -> bool`, field `pid: u32`.
- Produces:
  - `RunnerError::StillRunning { id: String, pid: u32 }`, displayed as `<id>: agent processes still running after stop (pid <pid>)`.
  - `pub const STOP_WAIT: Duration` (5 s) in `tmux.rs`, re-exported from the crate root.
  - `TmuxRunner::stop_wait: Duration`, a public field set to `STOP_WAIT` by `TmuxRunner::new`.
  - `stop_agent`, `stop_crew` and `ensure_agent` on a running window return `Ok` only when the pane processes they ended are gone.

- [ ] **Step 1: Write the failing tests**

In `crates/balerix-runtime/tests/tmux_it.rs`, extend the `use balerix_runtime::…` lines with `balerix_runtime::testing::pid_alive`, and append:

```rust
/// One agent window running `body` as its `launch.sh`, on its own server.
struct Pane {
    r: TmuxRunner,
    id: AgentId,
    plan: LaunchPlan,
    _server: KillServer,
    _root: balerix_runtime::testing::TempRoot,
}

fn pane(label: &str, body: &str) -> Option<Pane> {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("tmux", false));
        return None;
    };
    let root = support::temp_root(label);
    let socket = format!("balerix-test-{label}-{}", std::process::id());
    let guard = KillServer {
        tmux: tools.tmux.clone(),
        socket: socket.clone(),
    };
    let r = TmuxRunner::new(tools.tmux.clone(), socket);
    let id: AgentId = "f/c/a".parse().unwrap();
    let agent_dir = root.join("a");
    std::fs::create_dir_all(agent_dir.join("logs")).unwrap();
    let script = agent_dir.join("launch.sh");
    std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = LaunchPlan {
        cwd: agent_dir,
        env: BTreeMap::new(),
        argv: vec![],
        script,
    };
    r.ensure_crew(&id.crew_ref()).unwrap();
    r.ensure_agent(&id, &plan).unwrap();
    Some(Pane {
        r,
        id,
        plan,
        _server: guard,
        _root: root,
    })
}

fn running_pid(p: &Pane) -> u32 {
    match p.r.observe(&p.id.fleet).unwrap().get(&p.id) {
        Some(ProcessState::Running { pid }) => *pid,
        other => panic!("expected running, got {other:?}"),
    }
}

/// A pane process that takes half a second to die after the hangup: a
/// stand-in for the supervisor emptying its tree.
const SLOW_TO_DIE: &str = "trap 'sleep 0.5; exit 0' HUP\nwhile :; do sleep 0.1; done";

/// Spec N amendment §13.5: `stop_agent` returns when the pane's process
/// is gone, not when tmux has dropped the window.
#[test]
fn stop_waits_for_the_pane_process_and_a_dead_pane_stops_at_once() {
    let Some(p) = pane("stopwait", SLOW_TO_DIE) else {
        return;
    };
    let pid = running_pid(&p);
    p.r.stop_agent(&p.id).unwrap();
    assert!(!pid_alive(pid), "the pane process outlived stop_agent");

    // a window whose pane already exited has nothing to wait for
    p.r.ensure_agent(&p.id, &p.plan).unwrap();
    let pid = running_pid(&p);
    assert!(
        std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    wait_for(|| {
        matches!(
            p.r.observe(&p.id.fleet).unwrap().get(&p.id),
            Some(ProcessState::Exited { .. })
        )
    });
    let start = Instant::now();
    p.r.stop_agent(&p.id).unwrap();
    p.r.stop_agent(&p.id).unwrap(); // absent → ok
    assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());
}

#[test]
fn stop_crew_waits_for_every_pane_process() {
    let Some(p) = pane("stopcrew", SLOW_TO_DIE) else {
        return;
    };
    let pid = running_pid(&p);
    p.r.stop_crew(&p.id.crew_ref()).unwrap();
    assert!(!pid_alive(pid), "the pane process outlived stop_crew");
    p.r.stop_crew(&p.id.crew_ref()).unwrap(); // absent → ok
}

/// A restart must not start the new agent while the old one's processes
/// are still dying.
#[test]
fn a_restart_waits_for_the_old_pane_process() {
    let Some(p) = pane("restartwait", SLOW_TO_DIE) else {
        return;
    };
    let old = running_pid(&p);
    p.r.ensure_agent(&p.id, &p.plan).unwrap();
    assert!(!pid_alive(old), "the old pane process outlived the restart");
    assert_ne!(running_pid(&p), old);
}

/// NS-12: past the bound the call fails and names the pid.
#[test]
fn a_pane_process_that_ignores_the_hangup_fails_the_stop_after_the_bound() {
    let Some(mut p) = pane("stopbound", "trap '' HUP\nwhile :; do sleep 0.2; done") else {
        return;
    };
    p.r.stop_wait = Duration::from_millis(300);
    let pid = running_pid(&p);
    let start = Instant::now();
    let err = p.r.stop_agent(&p.id).unwrap_err();
    assert!(start.elapsed() >= Duration::from_millis(300));
    assert_eq!(
        err.to_string(),
        format!("f/c/a: agent processes still running after stop (pid {pid})")
    );
    assert!(pid_alive(pid));
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
}
```

In the `#[cfg(test)]` module at the bottom of `crates/balerix-runtime/src/tmux.rs`, add:

```rust
    #[test]
    fn live_panes_are_the_ones_not_dead() {
        assert_eq!(parse_live_panes("0\t41\n1\t42\n0\t43\n\n0\tx\n"), vec![41, 43]);
        assert!(parse_live_panes("").is_empty());
    }
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `mise x -- cargo nextest run -p balerix-runtime --test tmux_it stop restart`
Expected: does not compile (`stop_wait` is not a field of `TmuxRunner`). After Step 3's struct change alone, the three wait tests would fail with "the pane process outlived …".

- [ ] **Step 3: Add the error variant**

In `crates/balerix-core/src/ports.rs`, in `enum RunnerError`, after `Parse`:

```rust
    /// A stop that timed out: the pane's process is still there (Spec N
    /// amendment §13.5).
    #[error("{id}: agent processes still running after stop (pid {pid})")]
    StillRunning { id: String, pid: u32 },
```

- [ ] **Step 4: Make the runner wait**

In `crates/balerix-runtime/src/tmux.rs`:

Add to the imports: `use crate::supervise::ProcIdentity;`

After `SEND_KEYS_LIMIT`:

```rust
/// How long a stop waits for the pane processes it ended (Spec N
/// amendment NS-12): the supervisor's grace plus a margin.
pub const STOP_WAIT: Duration = Duration::from_secs(5);
const STOP_POLL: Duration = Duration::from_millis(10);
const PANE_FORMAT: &str = "#{pane_dead}\t#{pane_pid}";

/// The pids of the panes that are not dead, from `PANE_FORMAT` lines.
pub(crate) fn parse_live_panes(text: &str) -> Vec<u32> {
    text.lines()
        .filter_map(|l| l.split_once('\t'))
        .filter(|(dead, _)| *dead == "0")
        .filter_map(|(_, pid)| pid.parse().ok())
        .collect()
}
```

In `struct TmuxRunner`, after `socket`:

```rust
    /// `STOP_WAIT`; a field so a test can shorten it.
    pub stop_wait: Duration,
```

and in `TmuxRunner::new`, `stop_wait: STOP_WAIT,` after `socket: socket.into(),`.

In `impl TmuxRunner`, after `windows`:

```rust
    /// Blocks until every process in `procs` has exited (Spec N amendment
    /// §13.5). The supervisor in front of an agent exits only when its
    /// tree is empty, so its exit is what "stopped" means.
    fn wait_gone(&self, id: &str, procs: &[ProcIdentity]) -> Result<(), RunnerError> {
        let deadline = Instant::now() + self.stop_wait;
        loop {
            let Some(alive) = procs.iter().find(|p| !p.gone()) else {
                return Ok(());
            };
            if Instant::now() >= deadline {
                return Err(RunnerError::StillRunning {
                    id: id.to_string(),
                    pid: alive.pid,
                });
            }
            std::thread::sleep(STOP_POLL);
        }
    }
```

Replace `stop_agent`:

```rust
    fn stop_agent(&self, agent: &AgentId) -> Result<(), RunnerError> {
        let id = agent.to_string();
        let pane = match self
            .windows(&agent.crew_ref())?
            .and_then(|w| w.get(&agent.agent).copied())
        {
            Some(ProcessState::Running { pid }) => ProcIdentity::of(pid),
            _ => None,
        };
        self.run_optional(&id, &["kill-window", "-t", &Self::window_target(agent)])?;
        self.wait_gone(&id, pane.as_slice())
    }
```

In `stop_crew`, read the panes before the kills and wait after them. Insert before `let Some(text) = self.run_optional(&name, &["list-sessions", …` :

```rust
        // Grouped sessions share the crew's windows, so the crew
        // session's panes are all of them.
        let panes: Vec<ProcIdentity> = self
            .run_optional(
                &name,
                &[
                    "list-panes",
                    "-s",
                    "-t",
                    &Self::session_target(crew),
                    "-F",
                    PANE_FORMAT,
                ],
            )?
            .map(|text| parse_live_panes(&text))
            .unwrap_or_default()
            .into_iter()
            .filter_map(ProcIdentity::of)
            .collect();
```

and replace the function's final `Ok(())` with:

```rust
        self.wait_gone(&name, &panes)
```

(The early `return Ok(());` when `list-sessions` finds no server stays: there is nothing to wait for.)

In `ensure_agent`, replace the arm `Some(ProcessState::Running { .. }) => {}` with:

```rust
            Some(ProcessState::Running { pid }) => {
                // A restart: end the old process first and wait for it,
                // so the new agent never starts while the old tree is
                // still dying (Spec N amendment §13.5). Through the idle
                // placeholder, for the same reason as the arms above.
                let old = ProcIdentity::of(pid);
                self.run(
                    &id,
                    &[
                        "respawn-window",
                        "-k",
                        "-t",
                        &target,
                        "-c",
                        &cwd,
                        IDLE_ARGV[0],
                        IDLE_ARGV[1],
                        IDLE_ARGV[2],
                    ],
                )?;
                self.wait_gone(&id, old.as_slice())?;
            }
```

In `crates/balerix-runtime/src/lib.rs`, extend the tmux re-export:

```rust
pub use tmux::{ANCHOR_WINDOW, ATTACH_SESSION_PREFIX, STOP_WAIT, TmuxAttach, TmuxRunner};
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `mise x -- cargo nextest run -p balerix-runtime --test tmux_it` and `mise x -- cargo nextest run -p balerix-runtime --lib tmux`
Expected: every `tmux_it` test passes, the four new ones and the existing ones (a restart in `session_window_observe_exit_respawn_and_teardown` and the `send_*` tests now go through the waits). `live_panes_are_the_ones_not_dead` passes.

- [ ] **Step 6: Lint and commit**

Run: `mise run check`
Expected: passes. A non-exhaustive `match` on `RunnerError` anywhere is a compile error here: add the arm beside `Parse`'s, with the same handling.

```bash
git add crates/balerix-core/src/ports.rs crates/balerix-runtime/src/tmux.rs crates/balerix-runtime/src/lib.rs crates/balerix-runtime/tests/tmux_it.rs
git commit -m "fix(runtime): stop and restart wait for the pane's process to be gone (#107)"
```

---

### Task 4: `launch.sh` starts the agent under the wrapper

**Files:**
- Modify: `crates/balerix-runtime/src/launch.rs` (`render_launch` and its unit test), `crates/balerix-runtime/tests/snapshots/generated_golden__payments_generated.snap`
- Test: `crates/balerix/tests/cli_supervise.rs` (append)

**Interfaces:**
- Consumes: `balerix agent-supervise -- <argv…>` (Task 2); `TmuxRunner::{new, ensure_crew, ensure_agent, stop_agent, stop_crew}` with the waits of Task 3.
- Produces: every agent `launch.sh` and `LaunchPlan::argv` begin `<tools.balerix> agent-supervise -- <tools.nono> -s --log-file …`.

- [ ] **Step 1: Write the failing tests**

In `crates/balerix-runtime/src/launch.rs`, in `script_quotes_everything_and_carries_no_secret_inputs`, replace the assertion `assert!(script.contains("'/opt/nono' '-s' '--log-file'"));` with:

```rust
        assert!(
            script.contains("'/opt/balerix' 'agent-supervise' '--' '/opt/nono' '-s' '--log-file'"),
            "{script}"
        );
        assert_eq!(
            plan.argv[..4],
            ["/opt/balerix", "agent-supervise", "--", "/opt/nono"].map(String::from)
        );
```

Append to `crates/balerix/tests/cli_supervise.rs`:

```rust
// -- with tmux: #107 end to end (Spec N amendment §13.7) --

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use balerix_core::{AgentId, AgentRunner, LaunchPlan};
use balerix_runtime::TmuxRunner;

fn tmux() -> Option<PathBuf> {
    let found = std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("tmux"))
        .find(|p| p.is_file());
    if found.is_none() {
        if std::env::var_os("BALERIX_REQUIRE_TOOLS").is_some_and(|v| v == "1") {
            panic!("tmux is required (BALERIX_REQUIRE_TOOLS=1) but not available");
        }
        eprintln!("skip: tmux not available");
    }
    found
}

struct KillServer {
    tmux: PathBuf,
    socket: String,
}

impl Drop for KillServer {
    fn drop(&mut self) {
        let _ = Command::new(&self.tmux)
            .args(["-L", &self.socket, "kill-server"])
            .status();
    }
}

struct Supervised {
    r: TmuxRunner,
    id: AgentId,
    plan: LaunchPlan,
    _server: KillServer,
    _dir: tempfile::TempDir,
}

/// An agent window whose `launch.sh` is the wrapper in front of a script
/// that detaches one sleeper and stays in another.
fn supervised_agent(label: &str, m: &str) -> Option<Supervised> {
    let tmux = tmux()?;
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    std::fs::create_dir_all(dir.path().join("logs")).unwrap();
    let script = dir.path().join("launch.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nexec {BALERIX} agent-supervise -- /bin/sh -c 'setsid sleep {m} & exec sleep {m}'\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let socket = format!("balerix-test-{label}-{}", std::process::id());
    let guard = KillServer {
        tmux: tmux.clone(),
        socket: socket.clone(),
    };
    let r = TmuxRunner::new(tmux, socket);
    let id: AgentId = "f/c/a".parse().unwrap();
    let plan = LaunchPlan {
        cwd: dir.path().to_path_buf(),
        env: BTreeMap::new(),
        argv: vec![],
        script,
    };
    r.ensure_crew(&id.crew_ref()).unwrap();
    r.ensure_agent(&id, &plan).unwrap();
    wait_for(|| sleepers(m).len() == 2);
    Some(Supervised {
        r,
        id,
        plan,
        _server: guard,
        _dir: dir,
    })
}

/// #107's own case.
#[test]
fn stop_agent_leaves_no_process_of_an_agent_that_detached_a_sleeper() {
    let m = marker(11);
    let Some(a) = supervised_agent("sup-stop", &m) else {
        return;
    };
    a.r.stop_agent(&a.id).unwrap();
    assert!(with_cmdline(&m).is_empty(), "{:?}", with_cmdline(&m));
}

#[test]
fn stop_crew_leaves_no_process_of_an_agent_that_detached_a_sleeper() {
    let m = marker(12);
    let Some(a) = supervised_agent("sup-crew", &m) else {
        return;
    };
    a.r.stop_crew(&a.id.crew_ref()).unwrap();
    assert!(with_cmdline(&m).is_empty(), "{:?}", with_cmdline(&m));
}

#[test]
fn a_restart_leaves_no_process_of_the_old_agent() {
    let m = marker(13);
    let Some(a) = supervised_agent("sup-restart", &m) else {
        return;
    };
    let old = sleepers(&m);
    a.r.ensure_agent(&a.id, &a.plan).unwrap();
    assert!(
        old.iter().all(|pid| !pid_alive(*pid)),
        "a sleeper of the old agent outlived the restart"
    );
    wait_for(|| sleepers(&m).len() == 2);
    a.r.stop_crew(&a.id.crew_ref()).unwrap();
}
```

Move the three new `use` lines up beside the file's other imports (rustfmt will not do it for you).

- [ ] **Step 2: Run the tests to see what fails**

Run: `mise x -- cargo nextest run -p balerix-runtime --lib launch`
Expected: `script_quotes_everything_and_carries_no_secret_inputs` fails on the new assertion.

Run: `mise x -- cargo nextest run -p balerix --test cli_supervise sleeper restart`
Expected: the three tmux tests pass already (Tasks 2 and 3 give them everything; they pin the two together). Each takes a little over 2 s.

- [ ] **Step 3: Put the wrapper in front of nono**

In `crates/balerix-runtime/src/launch.rs`, in `render_launch`, the `argv` vector starts:

```rust
    let mut argv: Vec<String> = vec![
        // Spec N amendment §13.4: a child subreaper in front of nono, so
        // `stop` can end everything the agent detached.
        tools.balerix.display().to_string(),
        "agent-supervise".into(),
        "--".into(),
        tools.nono.display().to_string(),
```

(the rest of the vector is unchanged). Update the module doc's first line to read:

```rust
//! `launch.sh` and the `LaunchPlan` (Phase 2 spec §4.2 step 5; the
//! supervisor in front of nono, Spec N amendment §13.4). No credential
//! is ever an input here.
```

- [ ] **Step 4: Regenerate the golden snapshot**

Run: `INSTA_UPDATE=always mise x -- cargo nextest run -p balerix-runtime --test generated_golden`
Then: `git diff crates/balerix-runtime/tests/snapshots/`
Expected: exactly two changed lines in `generated_golden__payments_generated.snap`, one per agent's `launch.sh`, each gaining `'<balerix path>' 'agent-supervise' '--'` before the nono path. `plugin_golden__plugin_launch.snap` is unchanged. Anything else changing is a mistake: revert and look.

- [ ] **Step 5: Run the tests to see them pass**

Run: `mise x -- cargo nextest run -p balerix-runtime` and `mise x -- cargo nextest run -p balerix --test cli_supervise`
Expected: all pass.

- [ ] **Step 6: The journey with the real binary**

Run: `mise run e2e`
Expected: passes. The e2e launches agents through the real `launch.sh`, so this is the first run of the wrapper under tmux, nono and `mise exec` together. A failure here is a real finding: stop and report the output.

- [ ] **Step 7: Lint and commit**

Run: `mise run check`

```bash
git add crates/balerix-runtime/src/launch.rs crates/balerix-runtime/tests/snapshots crates/balerix/tests/cli_supervise.rs
git commit -m "fix(runtime): launch.sh starts the agent under agent-supervise (#107)"
```

---

### Task 5: The sandbox cannot signal the wrapper

**Files:**
- Test: `crates/balerix-runtime/tests/sandbox_it.rs` (`generated_profile_validates_and_enforces_isolation`)

**Interfaces:**
- Consumes: nothing from earlier tasks. The wrapper runs outside the sandbox as the same user; the test process stands in for it.

Spec §13.6 rests on this: an agent that could SIGKILL the wrapper would send its own orphans back to pid 1.

- [ ] **Step 1: Extend the test**

In `generated_profile_validates_and_enforces_isolation`, replace the `let script = format!(…);` statement with:

```rust
    // Spec N amendment §13.6: the supervisor sits outside the sandbox as
    // the same user. This test process stands in for it.
    let script = format!(
        "echo in > \"$HOME/ok\" && echo HOME=$HOME && echo FOO=$FOO \
         && (echo x > {outside}/nope 2>/dev/null && echo ESCAPED || echo denied) \
         && (cat {objects}/ab/probe 2>/dev/null || echo CACHE_UNREADABLE) \
         && (echo x > {objects}/nope 2>/dev/null && echo CACHE_WRITABLE || echo cache-denied) \
         && (kill -0 {outside_pid} 2>/dev/null && echo SIGNALLED || echo signal-denied)",
        outside = outside.display(),
        objects = objects.display(),
        outside_pid = std::process::id()
    );
```

and add, after the `cache-denied` assertion:

```rust
    assert!(
        stdout.contains("signal-denied") && !stdout.contains("SIGNALLED"),
        "a sandboxed process must not be able to signal one outside: {stdout}"
    );
```

- [ ] **Step 2: Run it**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --test sandbox_it generated_profile`
Expected: passes (the probe saw `Operation not permitted` under nono 0.79.0's default policy).

If it prints `SIGNALLED`: the agent's generated profile lets the agent signal the wrapper, and §13.6's claim is false for real agents. Stop, do not weaken the assertion, and report: the fix would be a profile change, which is outside this plan.

- [ ] **Step 3: Commit**

```bash
git add crates/balerix-runtime/tests/sandbox_it.rs
git commit -m "test(runtime): a sandboxed process cannot signal one outside the sandbox (#107)"
```

---

### Task 6: Documents, the by-hand check, the PR

**Files:**
- Modify: `docs/THREAT-MODEL.md`, `ARCHITECTURE.md`, `AGENTS.md`, `crates/balerix-runtime/src/materializer.rs` (two comments)

**Interfaces:**
- Consumes: everything above, merged on the branch.

- [ ] **Step 1: `docs/THREAT-MODEL.md`**

In the paragraph that begins `- **The daemon runs read-only \`git\` in a repository an agent can write to.**`, replace its last clause

```
; and a process the agent detached outlives `stop`, inside the agent's sandbox, until it exits.
```

with

```
. A process the agent detached no longer outlives `stop`: see the process-tree row below.
```

In the mitigations table, after the row that begins `| A viewer moving the operator's tmux client, or agents outliving \`down\``, add one row:

```
| A process an agent detached (`setsid`, a double fork) outliving `stop`, with network, and writing into `workspace/` while the daemon deletes it | `launch.sh` starts the agent under `balerix agent-supervise`, a child subreaper outside the sandbox: every orphan below it re-parents to it, and on the hangup (2 s grace) or the main child's own exit it SIGKILLs every descendant until it has no children; `stop_agent`, `stop_crew` and a restart return only when the pane's process (pid and start time) is gone, and fail with `agent processes still running after stop` after 5 s, so removal never reaches the workspace delete with the tree alive. Limits: a wrapper that is itself SIGKILLed (the OOM killer, an operator) leaves its orphans to pid 1, which the sandboxed agent cannot cause (`sandbox_it` asserts a signal to a process outside the sandbox is refused); the kill loop races a process that forks faster than one `/proc` scan (a cgroup would not, and is unavailable in an unprivileged container); an agent launched before the upgrade has no wrapper until its next launch; a timed-out stop is reported once, because the retry finds no window to wait on (Spec N amendment §13) | `crates/balerix-runtime/src/supervise.rs`, `tmux.rs::{stop_agent, stop_crew, ensure_agent}`, `launch.rs`; `crates/balerix/tests/cli_supervise.rs` |
```

- [ ] **Step 2: `ARCHITECTURE.md`**

Replace `` `nono run → mise exec → claude` `` (the tmux-window sentence near the top) with `` `balerix agent-supervise → nono run → mise exec → claude` ``.

In the paragraph that ends `` `AgentRunner` (`TmuxRunner`) makes processes: session per crew, window per agent, `remain-on-exit`, `respawn-window`. ``, insert before `` `balerix dev materialize` runs the file half alone. ``:

```
`agent-supervise` is a child subreaper in front of nono: `stop` and a
restart wait for it, and it exits only when nothing the agent started is
left (Spec N amendment §13).
```

- [ ] **Step 3: `AGENTS.md`**

Under `## Conventions`, add one bullet at the end of the list:

```
- An agent's `launch.sh` starts under `balerix agent-supervise` (Spec N
  amendment §13). `balerix-runtime`'s test support fills the `balerix`
  tool slot with the test executable, so a test there must not execute a
  rendered `launch.sh`; tests that need the wrapper live in
  `crates/balerix/tests/cli_supervise.rs`. Never call
  `balerix_runtime::supervise::supervise` inside a test process: it makes
  the process a subreaper and kills every descendant.
```

- [ ] **Step 4: The two comments in `materializer.rs`**

The doc comment on `retry_rmdir` says nono flushes "shortly after `tmux kill-window` returns". Replace that sentence's clause so the comment reads:

```rust
/// `remove_dir_all` with a bounded retry on `DirectoryNotEmpty`: nono
/// flushes its audit ledger and session file under `<root>/nono/` as it
/// exits. `stop` now waits for the pane's process (Spec N amendment
/// §13.5), so this only matters after a stop that timed out, or for a
/// writer that is not the agent's own tree; a tree that was quiet when
/// the walk started can still refill under it. `NotFound` is success;
/// every other error is returned at once, since only a concurrent writer
/// is worth waiting for.
```

and the comment on the test `rm_rf_retries_a_directory_that_refills_under_it`:

```rust
    /// A writer under `plugins/<name>/nono/` can outlast a stop that
    /// timed out, so a `remove_dir_all` that loses that race must be
    /// retried rather than reported (`purge` and `down --purge` both go
    /// through `rm_rf`).
```

- [ ] **Step 5: The whole suite**

Run, in this order:

```bash
mise run check
mise run plugins
BALERIX_REQUIRE_TOOLS=1 mise run test-it
mise run e2e
```

Expected: all pass. `plugins` is here because the plugin fleet's windows go through the same `stop_agent` and `stop_crew`.

- [ ] **Step 6: `verify-claude`, by hand**

Run: `mise run verify-claude`
Expected: every section passes, with the real `claude` now started as `balerix agent-supervise -- nono … claude`. Watch for: the TUI drawing normally (the wrapper inherits the terminal), `/exit` ending the session (section G), and `down` returning within a few seconds. Keep the report's text for the PR.

- [ ] **Step 7: Commit, push, open the PR**

```bash
git add docs/THREAT-MODEL.md ARCHITECTURE.md AGENTS.md crates/balerix-runtime/src/materializer.rs
git commit -m "docs: the process tree in the threat model and the architecture (#107)"
git push -u origin fix/stop-process-tree
gh pr create --title "fix(runtime): stop ends the sandbox's whole process tree and waits for it to be empty (#107)" --body-file <file>
```

The body: what changed (the wrapper, the waits, `launch.sh`), the limits of spec §13.6 in four lines, the results of Step 5 and Step 6, and `Closes #107`.
