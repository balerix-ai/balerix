# Daemon Mise Actor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the daemon pool a single owner — a `SystemPool` actor that
installs it at startup and re-checks it every resync tick — so N fleet actors
stop writing daemon-global state on parallel threads.

**Architecture:** A new `SystemToolchain` port in `balerix-core` exposes one
idempotent method, implemented by `Runtime`. A `SystemPool` actor in
`balerix-server` calls it in a sequential loop and publishes readiness on a
`watch` channel in `Shared`. Fleet actors check that channel at the top of
every pass and skip — never fail — while it is not ready. Failure is never
fatal: the actor logs, publishes the reason, and retries next tick.

**Tech Stack:** Rust 2024 edition, `tokio` (server only), `mise`,
`cargo-nextest`, `insta` snapshots.

**Spec:** `docs/superpowers/specs/2026-09-09-balerix-f-daemon-mise-actor-design.md`

## Global Constraints

- Ports live in `balerix-core` and stay **synchronous**. No `tokio` in
  `balerix-core`. `balerix-core` never depends on adapter crates.
- `balerix-server` receives ports and **never imports `balerix-runtime`**.
  Only the `balerix` binary wires adapters to ports.
- The reconciler's `plan()` is a pure function and must not be touched. Only
  the actor's pass entry changes.
- Nothing in `balerix-runtime` reads the process environment. Paths come from
  `StateLayout`, binaries from `ToolPaths`.
- **No new Cargo dependencies.** The `Cmd` timeout is built from `std` only.
- Readiness is a **live condition**, re-read at the top of every pass. Never a
  latch (Spec F, F-4).
- A failed system install is **never fatal**, at startup or later (F-7). No
  `std::process::exit`, no fatal channel, no change to `Daemon::start`'s
  signature.
- A gated fleet pass is **skipped, not failed**: no crew marked failed, no
  restart counter touched.
- Run `mise run check` before every commit. The pre-commit hook runs the full
  check itself, so redirect its output to a file and keep one commit per issue.
- Run cargo through mise (`mise x -- cargo …`) or via a `mise run` task.
- Integration tests that need real tools use `support::require_or_skip(...)`.
- A failure that reproduces every time is not a flake. Before blaming host
  load, re-run at the parent commit and compare.

---

### Task 1: A pipe-safe timeout on `Cmd`

**Files:**
- Modify: `crates/balerix-runtime/src/tools.rs`
- Test: `crates/balerix-runtime/src/tools.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `impl Cmd { pub fn timeout(self, d: std::time::Duration) -> Self }` —
    builder, same shape as the existing `log`/`cwd`/`envs`.
  - A timed-out run returns the existing `Failure` type with
    `stderr` containing `timed out after <n>s`.

**Why this is first:** Task 4's actor depends on it, and it is the only task
that touches a primitive every adapter already uses, so it gets its own
reviewer gate.

**The trap this avoids:** the obvious implementation — spawn, poll `try_wait`,
kill on expiry — deadlocks. The current code calls `output()`, which drains the
child's stdout and stderr; a child that fills a pipe buffer blocks forever
while the parent polls. Run the blocking capture on its own thread and wait on
a channel with a deadline instead.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/balerix-runtime/src/tools.rs`:

```rust
    #[test]
    fn a_timeout_kills_a_hung_child_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs").join("sleep.log");
        let start = std::time::Instant::now();
        let err = Cmd::new(Path::new("/bin/sh"))
            .args(["-c", "sleep 30"])
            .log(&log)
            .timeout(std::time::Duration::from_millis(300))
            .run()
            .expect_err("a 30s sleep under a 300ms timeout must fail");
        assert!(
            err.stderr.contains("timed out"),
            "expected a timeout message, got {:?}",
            err.stderr
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "the timeout must not wait out the child"
        );
    }

    #[test]
    fn a_chatty_child_does_not_deadlock_under_a_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs").join("chatty.log");
        // Far more than a pipe buffer (64 KiB on Linux): a implementation
        // that polls without draining the pipes hangs here forever.
        let out = Cmd::new(Path::new("/bin/sh"))
            .args(["-c", "yes balerix | head -c 400000"])
            .log(&log)
            .timeout(std::time::Duration::from_secs(30))
            .run()
            .expect("a fast chatty child must succeed well inside its timeout");
        assert!(out.stdout.len() >= 400_000, "stdout was truncated");
    }

    #[test]
    fn without_a_timeout_a_command_still_runs_to_completion() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs").join("plain.log");
        let out = Cmd::new(Path::new("/bin/sh"))
            .args(["-c", "echo ok"])
            .log(&log)
            .run()
            .expect("no timeout set: unchanged behaviour");
        assert_eq!(out.stdout.trim(), "ok");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-runtime --lib tools:: 2>&1 | tail -20`
Expected: FAIL, `no method named 'timeout' found for struct 'Cmd'`.

- [ ] **Step 3: Add the field and the builder**

Add `timeout: Option<std::time::Duration>` to the `Cmd` struct, defaulted to
`None` wherever `Cmd::new` builds it, and the builder beside `log`:

```rust
    /// Kills the child and fails if it outlives `d`. Only the daemon pool
    /// install sets this today (Spec F §6); every other call is unbounded as
    /// before.
    pub fn timeout(mut self, d: std::time::Duration) -> Self {
        self.timeout = Some(d);
        self
    }
```

- [ ] **Step 4: Write the pipe-safe wait**

In `run`, replace the `None => c.output().map_err(cannot_execute)?` arm with a
branch on `self.timeout`. Unchanged when `None`. When `Some(d)`:

```rust
            None => match self.timeout {
                None => c.output().map_err(cannot_execute)?,
                Some(d) => {
                    let mut child = c
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .map_err(cannot_execute)?;
                    let pid = child.id();
                    // `wait_with_output` drains both pipes; doing it on its
                    // own thread is what keeps a chatty child from filling a
                    // pipe buffer and deadlocking us while we wait.
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let _ = tx.send(child.wait_with_output());
                    });
                    match rx.recv_timeout(d) {
                        Ok(out) => out.map_err(cannot_execute)?,
                        Err(_) => {
                            // The reader thread owns the child, so kill by
                            // pid and let it observe the exit and finish.
                            kill_pid(pid);
                            return Err(failure(format!(
                                "timed out after {}s",
                                d.as_secs()
                            )));
                        }
                    }
                }
            },
```

The workspace forbids `unsafe`, so the kill goes through a process rather than
`libc::kill`:

```rust
/// `kill(1)` rather than a raw signal: the workspace forbids `unsafe`, and the
/// reader thread owns the `Child`, so `Child::kill` is not reachable here.
fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
}
```

If `kill(1)` is not on `PATH` in some environment this must still fail as a
timeout rather than hang, which it does: the error is already returned before
the kill's outcome is examined.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `mise x -- cargo test -p balerix-runtime --lib tools:: 2>&1 | tail -20`
Expected: PASS, all three new tests green.

- [ ] **Step 6: Run the full check and commit**

```bash
mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log
git add crates/balerix-runtime/src/tools.rs
git commit -m "Give Cmd an optional pipe-safe timeout"
```

---

### Task 2: The `SystemToolchain` port and its fake

**Files:**
- Modify: `crates/balerix-core/src/ports.rs`
- Modify: `crates/balerix-core/src/lib.rs` (export)
- Modify: `crates/balerix-core/src/fakes.rs` (add `FakeSystemToolchain`)
- Test: `crates/balerix-core/src/fakes.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub trait SystemToolchain: Send + Sync { fn ensure_system_pool(&self) -> Result<(), MaterializeError>; }`
  - `pub struct FakeSystemToolchain` with:
    - `pub fn ready() -> Self` — always `Ok`
    - `pub fn failing(times: usize) -> Self` — fails `times` times, then `Ok`
    - `pub fn always_failing() -> Self`
    - `pub fn calls(&self) -> usize`

**Note:** the trait is synchronous, like every other port. The actor calls it
on a blocking thread.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/balerix-core/src/fakes.rs`:

```rust
    #[test]
    fn the_fake_system_toolchain_counts_calls_and_recovers_after_n_failures() {
        let tc = FakeSystemToolchain::failing(2);
        assert!(tc.ensure_system_pool().is_err(), "first attempt fails");
        assert!(tc.ensure_system_pool().is_err(), "second attempt fails");
        assert!(tc.ensure_system_pool().is_ok(), "third attempt succeeds");
        assert_eq!(tc.calls(), 3);

        let always = FakeSystemToolchain::always_failing();
        assert!(always.ensure_system_pool().is_err());
        assert!(always.ensure_system_pool().is_err());
        assert_eq!(always.calls(), 2);

        let ok = FakeSystemToolchain::ready();
        assert!(ok.ensure_system_pool().is_ok());
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `mise x -- cargo test -p balerix-core --lib fakes:: 2>&1 | tail -20`
Expected: FAIL, `cannot find struct 'FakeSystemToolchain'`.

- [ ] **Step 3: Add the port**

In `crates/balerix-core/src/ports.rs`, beside `Materializer`:

```rust
/// Installs the daemon-level tool pool. Owned by one actor, never by the
/// reconciler (Spec F, F-1). Implementations must be idempotent and cheap
/// when nothing has changed: `Ok(())` means the pool matches the system
/// table *and* exists on disk.
pub trait SystemToolchain: Send + Sync {
    fn ensure_system_pool(&self) -> Result<(), MaterializeError>;
}
```

Add `SystemToolchain` to the `pub use ports::{…}` list in
`crates/balerix-core/src/lib.rs`, keeping it alphabetically placed.

- [ ] **Step 4: Add the fake**

In `crates/balerix-core/src/fakes.rs`:

```rust
/// A `SystemToolchain` that fails a fixed number of times before succeeding.
/// `calls()` is how the actor's retry behaviour is asserted.
pub struct FakeSystemToolchain {
    remaining_failures: Mutex<usize>,
    forever: bool,
    calls: Mutex<usize>,
}

impl FakeSystemToolchain {
    pub fn ready() -> Self {
        Self { remaining_failures: Mutex::new(0), forever: false, calls: Mutex::new(0) }
    }
    pub fn failing(times: usize) -> Self {
        Self { remaining_failures: Mutex::new(times), forever: false, calls: Mutex::new(0) }
    }
    pub fn always_failing() -> Self {
        Self { remaining_failures: Mutex::new(0), forever: true, calls: Mutex::new(0) }
    }
    pub fn calls(&self) -> usize {
        *lock(&self.calls)
    }
}

impl SystemToolchain for FakeSystemToolchain {
    fn ensure_system_pool(&self) -> Result<(), MaterializeError> {
        *lock(&self.calls) += 1;
        let mut left = lock(&self.remaining_failures);
        if self.forever || *left > 0 {
            *left = left.saturating_sub(1);
            return Err(MaterializeError::Invalid {
                id: "system".to_string(),
                message: "fake system pool failure".to_string(),
            });
        }
        Ok(())
    }
}
```

Reuse the existing `lock` helper at the top of `fakes.rs`; do not add another.

- [ ] **Step 5: Run the test to verify it passes**

Run: `mise x -- cargo test -p balerix-core --lib fakes:: 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 6: Run the full check and commit**

```bash
mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log
git add crates/balerix-core/src/ports.rs crates/balerix-core/src/lib.rs crates/balerix-core/src/fakes.rs
git commit -m "Add the SystemToolchain port and its fake"
```

---

### Task 3: `Runtime` implements the port; the system level leaves `install_pools`

**Files:**
- Modify: `crates/balerix-runtime/src/materializer.rs`
- Modify: `crates/balerix-runtime/tests/materialize_it.rs`
- Test: `crates/balerix-runtime/src/materializer.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `SystemToolchain` (Task 2), `Cmd::timeout` (Task 1, used here only
  to pass a timeout through — the actor chooses the value).
- Produces:
  - `impl SystemToolchain for Runtime` — renders the system table,
    `install_level`s it into the daemon pool with no parents, and verifies the
    pool directory exists.
  - `Runtime::install_pools` now installs **fleet and crew only**.

**Two behaviour changes the spec requires here:**

1. The install log moves from the triggering crew's
   `logs/mise.pools.log` to the daemon's own
   `server_dir()/logs/mise.system.log` (Spec F §3).
2. `Ok(())` must mean the pool *is* ready, so a marker that matches while the
   pool directory is missing counts as stale and reinstalls (F-5). The marker
   alone is not enough.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/balerix-runtime/src/materializer.rs`:

```rust
    #[test]
    fn a_matching_marker_with_no_pool_is_treated_as_stale() {
        let root = crate::testing::TempRoot::new("system-pool-stale");
        let layout = StateLayout::from_env(root.path(), |_| None);
        let rt = Runtime::new(layout.clone(), fake_tool_paths());

        // Write a marker holding the digest of the table that would be
        // rendered, but never create the pool.
        let text = render_level_toml("system", &system_tools(&layout, "system").unwrap());
        let digest = hex::encode(sha2::Sha256::digest(text.as_bytes()));
        std::fs::create_dir_all(layout.system_installed_marker().parent().unwrap()).unwrap();
        std::fs::write(layout.system_installed_marker(), &digest).unwrap();
        assert!(!layout.mise_data_dir().exists(), "no pool yet");

        // With a fake mise that cannot really install, the call must still
        // *attempt* it rather than short-circuit on the marker.
        let err = rt.ensure_system_pool().unwrap_err();
        assert!(
            !matches!(err, MaterializeError::Invalid { .. }),
            "a missing pool must drive a real install attempt, got {err:?}"
        );
    }
```

Adapt `fake_tool_paths()` to whatever the surrounding `mod tests` already uses
to build a `ToolPaths` pointing at a non-existent or stub binary; if no such
helper exists, point `ToolPaths::mise` at `/nonexistent/mise` so the install
fails as a `Tool` error rather than succeeding.

Add to `crates/balerix-runtime/tests/materialize_it.rs`:

```rust
#[test]
fn ensure_crew_no_longer_writes_the_daemon_pool() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise", false));
        return;
    };
    let root = support::temp_root("no-daemon-pool");
    let layout = support::layout(&root);
    let rt = Runtime::new(layout.clone(), tools);
    let daemon_pool = layout.mise_data_dir();
    std::fs::create_dir_all(&daemon_pool).unwrap();

    let before = pool_snapshot(&daemon_pool);
    let crew: CrewRef = "f/c".parse().unwrap();
    let (f, c) = (BTreeMap::new(), BTreeMap::new());
    let _ = rt.install_pools(&crew, balerix_core::CrewTools { fleet: &f, crew: &c });
    assert_eq!(
        pool_snapshot(&daemon_pool),
        before,
        "install_pools must not touch the daemon pool any more (Spec F §3)"
    );
}

/// Every path under `dir`, sorted, so an unchanged pool compares equal.
fn pool_snapshot(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            out.push(p.display().to_string());
            if p.is_dir() {
                stack.push(p);
            }
        }
    }
    out.sort();
    out
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-runtime materializer:: 2>&1 | tail -20`
Expected: FAIL, `no method named 'ensure_system_pool'`.

- [ ] **Step 3: Implement the port**

In `crates/balerix-runtime/src/materializer.rs`, add:

```rust
impl SystemToolchain for Runtime {
    /// The daemon pool, owned by the `SystemPool` actor (Spec F §3). Returns
    /// `Ok` only when the pool matches the system table *and* exists: a marker
    /// that survived a deleted pool would otherwise report ready over an empty
    /// directory (F-5).
    fn ensure_system_pool(&self) -> Result<(), MaterializeError> {
        let tc = Toolchain {
            tools: &self.tools,
            layout: &self.layout,
        };
        let pool = self.layout.mise_data_dir();
        let marker = self.layout.system_installed_marker();
        let log = self.layout.server_dir().join("logs").join("mise.system.log");
        let system = system_tools(&self.layout, "system")?;
        if !pool.exists() {
            // Drop a stale marker so `install_level` cannot short-circuit
            // past a pool that is no longer there.
            let _ = std::fs::remove_file(&marker);
        }
        tc.install_level(
            "system",
            "system",
            &self.layout.system_mise_toml_generated(),
            &pool,
            &[],
            &marker,
            &system,
            &log,
        )
    }
}
```

`install_level`'s first parameter is the crew id used in error messages; there
is no crew here, so `"system"` is passed for both it and the label, giving the
error id `system: system`. Collapse that to a single `"system"` in
`install_level` when `crew == label`, or accept the doubled form — pick one and
say which in your report.

- [ ] **Step 4: Drop the system level from `install_pools`**

Delete the first `tc.install_level(...)` call in `Runtime::install_pools` — the
one whose label is `"system"` — along with the now-unused `system` binding.
The fleet and crew calls are unchanged, including the fleet level still naming
`daemon_pool` as its parent. Add to the method's doc comment:

```rust
    /// The two fleet-owned pools, outermost first. The daemon pool is not
    /// installed here: it belongs to the `SystemPool` actor (Spec F §3).
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `mise x -- cargo test -p balerix-runtime 2>&1 | tail -20`
Expected: PASS.

Run: `BALERIX_REQUIRE_TOOLS=1 mise run test-it 2>&1 | tail -20`
Expected: PASS, including the new `ensure_crew_no_longer_writes_the_daemon_pool`.

- [ ] **Step 6: Run the full check and commit**

```bash
mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log
git add -A
git commit -m "Move the system pool install behind the SystemToolchain port"
```

---

### Task 4: The `SystemPool` actor

**Files:**
- Create: `crates/balerix-server/src/system_pool.rs`
- Modify: `crates/balerix-server/src/lib.rs` (declare and export the module)
- Test: `crates/balerix-server/src/system_pool.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `SystemToolchain` and `FakeSystemToolchain` (Task 2).
- Produces:
  - `pub enum SystemPoolState { Pending, Ready, Unready { reason: String } }`
    — deriving `Debug, Clone, PartialEq, Eq`.
  - `pub struct SystemPoolConfig { pub tick: Duration, pub attempt_timeout: Duration }`
    with a `Default` of `tick: 30s`, `attempt_timeout: 600s`.
  - `pub fn spawn(toolchain: Arc<dyn SystemToolchain>, tx: watch::Sender<SystemPoolState>, config: SystemPoolConfig) -> tokio::task::JoinHandle<()>`

**The loop, stated exactly (Spec F §6):** one attempt per tick, sequential, no
overlap, retried indefinitely. Never fatal, at startup or later. The first
attempt runs immediately rather than waiting out a tick.

**Note on the timeout:** `attempt_timeout` is carried in the config so the
actor owns the policy, but the enforcement lives in `Cmd::timeout` (Task 1)
inside the runtime's implementation. This task does not wrap the call in
`tokio::time::timeout`: abandoning a blocking call would leave the install
running while the actor believed it had stopped. Wiring `attempt_timeout` down
into the runtime is out of scope for this plan — record it as a follow-up in
your report and leave the field carried but unused, documented as such.

- [ ] **Step 1: Write the failing tests**

Create `crates/balerix-server/src/system_pool.rs` with only its `mod tests`
plus `use` lines, so the tests fail to compile against a missing `spawn`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use balerix_core::fakes::FakeSystemToolchain;
    use std::sync::Arc;
    use std::time::Duration;

    fn fast() -> SystemPoolConfig {
        SystemPoolConfig {
            tick: Duration::from_millis(20),
            attempt_timeout: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn a_healthy_pool_goes_pending_then_ready_and_stays_ready() {
        let tc = Arc::new(FakeSystemToolchain::ready());
        let (tx, mut rx) = tokio::sync::watch::channel(SystemPoolState::Pending);
        assert_eq!(*rx.borrow(), SystemPoolState::Pending);
        let handle = spawn(tc, tx, fast());

        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), SystemPoolState::Ready);

        // Several more ticks must not disturb it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(*rx.borrow(), SystemPoolState::Ready);
        handle.abort();
    }

    #[tokio::test]
    async fn a_failing_pool_publishes_the_reason_and_recovers_without_dying() {
        let tc = Arc::new(FakeSystemToolchain::failing(2));
        let (tx, mut rx) = tokio::sync::watch::channel(SystemPoolState::Pending);
        let handle = spawn(tc.clone(), tx, fast());

        rx.changed().await.unwrap();
        match &*rx.borrow() {
            SystemPoolState::Unready { reason } => {
                assert!(!reason.is_empty(), "an unready state must carry a reason")
            }
            other => panic!("expected Unready first, got {other:?}"),
        }

        // It keeps trying and eventually succeeds — no fatal path (F-7).
        loop {
            rx.changed().await.unwrap();
            if *rx.borrow() == SystemPoolState::Ready {
                break;
            }
        }
        assert!(tc.calls() >= 3, "one attempt per tick until it succeeds");
        assert!(!handle.is_finished(), "the actor must not exit on failure");
        handle.abort();
    }

    #[tokio::test]
    async fn readiness_drops_again_when_a_later_attempt_fails() {
        // Ready, then failing: proves readiness is live, not a latch (F-4).
        let tc = Arc::new(FakeSystemToolchain::ready_then_failing(1));
        let (tx, mut rx) = tokio::sync::watch::channel(SystemPoolState::Pending);
        let handle = spawn(tc, tx, fast());

        loop {
            rx.changed().await.unwrap();
            if *rx.borrow() == SystemPoolState::Ready {
                break;
            }
        }
        loop {
            rx.changed().await.unwrap();
            if matches!(*rx.borrow(), SystemPoolState::Unready { .. }) {
                break;
            }
        }
        handle.abort();
    }

    #[tokio::test]
    async fn a_permanently_failing_pool_never_terminates_the_actor() {
        let tc = Arc::new(FakeSystemToolchain::always_failing());
        let (tx, _rx) = tokio::sync::watch::channel(SystemPoolState::Pending);
        let handle = spawn(tc.clone(), tx, fast());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!handle.is_finished(), "never fatal (F-7)");
        assert!(tc.calls() >= 3, "it keeps retrying");
        handle.abort();
    }
}
```

This needs one more fake constructor. Add to `FakeSystemToolchain` in
`crates/balerix-core/src/fakes.rs`:

```rust
    /// Succeeds `ok` times, then fails forever. Drives the "readiness is not
    /// a latch" test (Spec F, F-4).
    pub fn ready_then_failing(ok: usize) -> Self {
        Self {
            remaining_failures: Mutex::new(0),
            forever: false,
            calls: Mutex::new(0),
            succeed_first: Mutex::new(ok),
        }
    }
```

and a `succeed_first: Mutex<usize>` field, defaulted to `usize::MAX` in the
other three constructors, consumed at the top of `ensure_system_pool`:

```rust
        let mut first = lock(&self.succeed_first);
        if *first > 0 && *first != usize::MAX {
            *first -= 1;
            return Ok(());
        }
        if *first == 0 {
            return Err(MaterializeError::Invalid {
                id: "system".to_string(),
                message: "fake system pool failure".to_string(),
            });
        }
        drop(first);
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-server --lib system_pool:: 2>&1 | tail -20`
Expected: FAIL, `cannot find function 'spawn' in this scope`.

- [ ] **Step 3: Write the actor**

Above the `mod tests` in `crates/balerix-server/src/system_pool.rs`:

```rust
//! The daemon pool's single owner (Spec F). One actor, one writer: fleet
//! actors used to install the system level from N parallel threads.

use std::sync::Arc;
use std::time::Duration;

use balerix_core::SystemToolchain;
use tokio::sync::watch;

/// What the daemon pool is doing. Read live at the top of every fleet pass —
/// never latched (Spec F, F-4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemPoolState {
    Pending,
    Ready,
    Unready { reason: String },
}

#[derive(Debug, Clone, Copy)]
pub struct SystemPoolConfig {
    /// How often the pool is re-checked. A matching marker makes this a file
    /// read and a hash, so it is cheap.
    pub tick: Duration,
    /// Per-attempt budget. Carried here so the policy lives with the actor;
    /// enforcement is `Cmd::timeout` inside the runtime.
    pub attempt_timeout: Duration,
}

impl Default for SystemPoolConfig {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(30),
            attempt_timeout: Duration::from_secs(600),
        }
    }
}

/// Runs until aborted. One attempt per tick, sequential, retried forever; a
/// failure is published, never fatal (F-7).
pub fn spawn(
    toolchain: Arc<dyn SystemToolchain>,
    tx: watch::Sender<SystemPoolState>,
    config: SystemPoolConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let tc = toolchain.clone();
            // The port is synchronous; a pass must not block the runtime.
            let outcome = tokio::task::spawn_blocking(move || tc.ensure_system_pool()).await;
            let next = match outcome {
                Ok(Ok(())) => SystemPoolState::Ready,
                Ok(Err(e)) => SystemPoolState::Unready {
                    reason: e.to_string(),
                },
                Err(e) => SystemPoolState::Unready {
                    reason: format!("system pool task failed: {e}"),
                },
            };
            if let SystemPoolState::Unready { reason } = &next {
                tracing::warn!(%reason, "daemon mise pool is not ready");
            }
            // `send_if_modified` keeps `changed()` meaningful: only a real
            // transition wakes the fleet actors waiting on it.
            tx.send_if_modified(|cur| {
                if *cur == next {
                    false
                } else {
                    *cur = next.clone();
                    true
                }
            });
            tokio::time::sleep(config.tick).await;
        }
    })
}
```

- [ ] **Step 4: Declare the module**

In `crates/balerix-server/src/lib.rs`, add `pub mod system_pool;` beside the
other module declarations, and re-export the two public types beside the
existing re-exports:

```rust
pub use system_pool::{SystemPoolConfig, SystemPoolState};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `mise x -- cargo test -p balerix-server --lib system_pool:: 2>&1 | tail -20`
Expected: PASS, all four tests.

- [ ] **Step 6: Run the full check and commit**

```bash
mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log
git add -A
git commit -m "Add the SystemPool actor"
```

---

### Task 5: Readiness in `Shared` and the fleet gate

**Files:**
- Modify: `crates/balerix-server/src/actor.rs`
- Test: `crates/balerix-server/src/actor.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `SystemPoolState` (Task 4).
- Produces:
  - `Shared` gains `pub system_pool: watch::Receiver<SystemPoolState>`.
  - `actor::shared(metrics)` returns
    `(Shared, mpsc::Receiver<FleetName>, watch::Sender<SystemPoolState>)` —
    a third element, the sender the `SystemPool` actor is spawned with.
  - Fleet passes are gated: a pass whose `system_pool` is not `Ready` is
    skipped without touching status.

**Why `Shared` and not `Ports`:** `Ports` is built at five call sites and
`PluginHost::start` rebuilds it field by field. `Shared` is created by
`actor::shared()` inside `Daemon::start` — exactly where the actor is spawned —
and the plugin host forwards it untouched. No construction site changes.

**The gate's two rules, both load-bearing:**
1. Not ready means **skip, not fail**. No crew marked failed, no restart
   counter touched — a fleet is not at fault for a daemon-level condition.
2. A skipped pass waits on **whichever comes first**: readiness changing, or
   the next tick. Waiting only for the tick would leave every fleet idle for a
   full resync interval after a pool that became ready two seconds in.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/balerix-server/src/actor.rs`:

```rust
    #[tokio::test]
    async fn a_pass_is_skipped_while_the_system_pool_is_not_ready() {
        let h = harness_with_system_pool(SystemPoolState::Pending);
        h.tick().await;
        assert_eq!(
            h.materializer_calls(),
            0,
            "an unready pool must stop the pass before it materializes anything"
        );
        assert!(
            h.status_unchanged(),
            "a skipped pass is not a failed pass: no crew failed, no counter moved"
        );
    }

    #[tokio::test]
    async fn a_pass_runs_once_the_system_pool_is_ready() {
        let h = harness_with_system_pool(SystemPoolState::Ready);
        h.tick().await;
        assert!(h.materializer_calls() > 0, "a ready pool lets the pass run");
    }

    #[tokio::test]
    async fn readiness_is_re_read_every_pass_not_latched() {
        let h = harness_with_system_pool(SystemPoolState::Ready);
        h.tick().await;
        let after_first = h.materializer_calls();
        assert!(after_first > 0);

        h.set_system_pool(SystemPoolState::Unready {
            reason: "gone".to_string(),
        });
        h.tick().await;
        assert_eq!(
            h.materializer_calls(),
            after_first,
            "readiness is live: a pool that goes away must stop later passes too (F-4)"
        );
    }
```

Adapt `harness_with_system_pool`, `tick`, `materializer_calls`,
`status_unchanged` and `set_system_pool` to the shapes the existing `mod tests`
in `actor.rs` already provides — read the neighbouring tests first and follow
their idiom. The assertions above are the point and must not weaken; the
plumbing around them is whatever that module already does.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-server --lib actor:: 2>&1 | tail -20`
Expected: FAIL, `no field 'system_pool' on type 'Shared'`.

- [ ] **Step 3: Add the channel to `Shared`**

```rust
pub struct Shared {
    pub hook_secrets: SecretIndex,
    pub metrics: Metrics,
    pub purged: mpsc::Sender<FleetName>,
    /// The daemon pool's live state. Read at the top of every pass; a pass
    /// does not run unless this is `Ready` (Spec F §5).
    pub system_pool: watch::Receiver<SystemPoolState>,
}

pub fn shared(
    metrics: Metrics,
) -> (
    Shared,
    mpsc::Receiver<FleetName>,
    watch::Sender<SystemPoolState>,
) {
    let (purged, rx) = mpsc::channel(16);
    let (pool_tx, pool_rx) = watch::channel(SystemPoolState::Pending);
    (
        Shared {
            hook_secrets: Arc::default(),
            metrics,
            purged,
            system_pool: pool_rx,
        },
        rx,
        pool_tx,
    )
}
```

- [ ] **Step 4: Gate the pass**

At the top of the actor's pass, before any reconciliation work:

```rust
        // Spec F §5: the daemon pool is a precondition, not the fleet's
        // fault. Skip the pass and leave status alone; a crew marked failed
        // here would move restart counters for a daemon-level condition.
        if *self.shared.system_pool.borrow() != SystemPoolState::Ready {
            tracing::debug!(fleet = %self.name, "skipping the pass: daemon mise pool not ready");
            return;
        }
```

Place it so a skipped pass still reschedules. In the actor's wait, select on
the readiness channel alongside the existing timer so a pool that becomes ready
wakes the fleet immediately:

```rust
            tokio::select! {
                _ = tokio::time::sleep_until(next) => {}
                _ = self.shared.system_pool.changed() => {}
                msg = self.rx.recv() => { /* existing arm, unchanged */ }
            }
```

Adapt to the actual shape of the existing select; the requirement is that
readiness changing is one of the things that can wake it.

- [ ] **Step 5: Fix the `shared()` call sites**

`actor::shared()` now returns three values. Update every caller — the compiler
will find them. Bind the new sender and drop it where the pool actor is not
being started yet (Task 6 uses it in `Daemon::start`); a dropped sender would
close the channel, so bind it to a named variable that outlives the test rather
than to `_`.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `mise x -- cargo test -p balerix-server 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 7: Run the full check and commit**

```bash
mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log
git add -A
git commit -m "Gate every fleet pass on the daemon pool's live readiness"
```

---

### Task 6: Start the actor and wire the port

**Files:**
- Modify: `crates/balerix-server/src/daemon.rs`
- Modify: `crates/balerix/src/commands/serve.rs`
- Modify: `crates/balerix-server/src/testing.rs`
- Test: `crates/balerix-server/src/daemon.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `system_pool::spawn` and `SystemPoolConfig` (Task 4),
  `actor::shared`'s third return value (Task 5), `SystemToolchain` (Task 2),
  `impl SystemToolchain for Runtime` (Task 3).
- Produces: a running daemon whose pool is owned by the actor.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/balerix-server/src/daemon.rs`:

```rust
    #[tokio::test]
    async fn start_spawns_the_system_pool_actor_and_fleets_wait_for_it() {
        // A toolchain that fails once then succeeds: the daemon must come up
        // regardless, and fleets must reconcile only after it goes ready.
        let tc = Arc::new(FakeSystemToolchain::failing(1));
        let d = test_daemon_with_system_toolchain(tc.clone()).await;
        assert!(d.is_serving(), "the API serves immediately (F-3)");
        eventually(|| tc.calls() >= 1).await;
    }
```

Adapt `test_daemon_with_system_toolchain`, `is_serving` and `eventually` to the
helpers `daemon.rs`'s tests already use; if there is no `eventually`, poll in a
short loop with a bounded deadline rather than a bare sleep.

- [ ] **Step 2: Run the test to verify it fails**

Run: `mise x -- cargo test -p balerix-server --lib daemon:: 2>&1 | tail -20`
Expected: FAIL, `Daemon::start` takes no system toolchain.

- [ ] **Step 3: Start the actor in `Daemon::start`**

`Daemon::start` gains one parameter, `system_toolchain: Arc<dyn SystemToolchain>`,
and spawns the actor with the sender `shared()` now returns:

```rust
        let (shared, purged, pool_tx) = actor::shared(metrics);
        // Spec F §4: one owner for the daemon pool. Spawned before the fleet
        // actors, though they gate on readiness rather than on spawn order.
        crate::system_pool::spawn(system_toolchain, pool_tx, SystemPoolConfig::default());
```

`Daemon::start` already carries `#[allow(clippy::too_many_arguments)]`; keep it.
Its signature stays infallible — nothing here can fail (F-7).

- [ ] **Step 4: Wire the binary**

In `crates/balerix/src/commands/serve.rs`, `runtime` is already an
`Arc<Runtime>` used for both `materializer` and `workspace`. Pass it once more:

```rust
        let daemon = Daemon::start(
            ports,
            handler,
            metrics,
            token,
            existing,
            plugin_config,
            registry,
            client,
            kv,
            runtime.clone(),
        );
```

Match the parameter's actual position in the signature you wrote in Step 3.
Update `crates/balerix-server/src/testing.rs`'s `Daemon::start` calls the same
way, passing `Arc::new(FakeSystemToolchain::ready())` so existing tests are
unaffected by the gate.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `mise x -- cargo test --workspace 2>&1 | tail -20`
Expected: PASS.

Run: `BALERIX_REQUIRE_TOOLS=1 mise run test-it 2>&1 | tail -20`
Expected: PASS.

Run: `mise run e2e 2>&1 | tail -20`
Expected: PASS. The journey exercises a real daemon start with the gate in
place; if it hangs, the gate is not going ready and that is a real bug, not a
flake.

- [ ] **Step 6: Run the full check and commit**

```bash
mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log
git add -A
git commit -m "Own the daemon pool from Daemon::start"
```

---

### Task 7: Documentation

**Files:**
- Modify: `ARCHITECTURE.md`
- Modify: `docs/THREAT-MODEL.md`

**Interfaces:**
- Consumes: the behaviour built in Tasks 1 through 6.
- Produces: no code interface.

- [ ] **Step 1: Update ARCHITECTURE.md**

The Spec E pool-hierarchy bullet says tools are "installed by the daemon into
that level's own read-only mise data dir". Extend it so the daemon level's
owner is named:

```markdown
- **The daemon pool has one owner.** The system table is installed by a
  `SystemPool` actor at daemon start and re-checked each resync tick, not by
  whichever crew's pass reached it first. Its readiness is a live condition:
  a fleet pass is skipped, not failed, while the pool is unready, and a pool
  deleted underneath a running daemon is noticed at the next tick. A failed
  install is never fatal — the daemon stays up, publishes the reason and
  retries.
```

- [ ] **Step 2: Update docs/THREAT-MODEL.md**

Spec E added an accepted-risk line about two fleets racing on the system-level
pool install. **Delete it** — with a single writer the race cannot occur — and
replace it with the narrower residual:

```markdown
- **A plugin install and the daemon pool actor can still contend.** Plugin
  installs run `mise install` with the daemon pool as their `MISE_DATA_DIR`
  and stay outside the `SystemPool` actor (Spec F, F-8). They do not share a
  config path, so only the install contention applies, not the temp-file
  collision Spec F removed. The loser retries.
```

- [ ] **Step 3: Verify the documentation against the code**

Run: `grep -rn "two fleets\|cross-fleet\|races on the system" ARCHITECTURE.md docs/THREAT-MODEL.md`
Expected: no surviving claim that two fleets can race on the system pool.

Run: `grep -rn "ensure_crew" ARCHITECTURE.md docs/THREAT-MODEL.md`
Expected: no claim that the system level installs in `ensure_crew`.

- [ ] **Step 4: Run the full check and commit**

```bash
mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log
git add -A
git commit -m "Document the daemon pool's single owner"
```

---

## Self-Review

**Spec coverage.** §3 the port and what moves → Tasks 2 and 3. §4 the actor →
Task 4. §5 readiness and the fleet gate → Task 5. §6 timeout and retries →
Task 1 (the timeout primitive) and Task 4 (the loop). §7 testing → the test
steps of every task, with the integration assertion in Task 3 and the e2e run
in Task 6. §8 corrections → Task 7. §10 "Done when" → Task 6's e2e plus Task
3's integration test plus Task 4's unit tests.

**Two gaps found and closed while reviewing.**

The spec says readiness lives in `Ports`; planning showed `Shared` is the right
home, because `Ports` is built at five call sites and `PluginHost::start`
rebuilds it field by field, while `Shared` is created inside `Daemon::start`
where the actor is spawned. The spec was corrected rather than letting the plan
diverge from it.

`SystemPoolConfig::attempt_timeout` is carried but not yet threaded into the
runtime's `install_level` call, because doing so means passing a timeout
through `Toolchain` and `Cmd` from the actor's config — a wider change than the
gate itself. Task 4 says so explicitly and asks the implementer to record it as
a follow-up rather than quietly leaving a dead field.

**One behaviour worth re-reading before implementing.** Task 5's gate must skip
a pass *without* touching status. It is tempting to reuse the existing failure
path, which would mark crews failed and move restart counters for a
daemon-level condition that has nothing to do with the fleet. The test
`a_pass_is_skipped_while_the_system_pool_is_not_ready` asserts both halves on
purpose.
