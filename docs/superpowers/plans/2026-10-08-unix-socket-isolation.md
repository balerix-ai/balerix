# Unix Socket Isolation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop sandboxed agents, plugins and the daemon's git from reaching Unix sockets outside their grants (the daemon's tmux server above all), while keeping an agent's own sockets working where the host allows it.

**Architecture:** `serve` (and the pod sidecar) resolve a `SocketPolicy` at start-up from `[sandbox] unix_sockets` and a probe run under nono. `Mediate` adds nono's `linux.af_unix_mediation: "pathname"` plus `unix_socket_subtree_bind` on each sandbox's writable directories; `Deny` wraps the sandboxed command in `balerix sandbox-exec`, which installs a seccomp filter failing `socket(AF_UNIX, …)`; `Open` is today's behaviour.

**Tech Stack:** Rust (workspace crates `balerix`, `balerix-runtime`, `agent`), nono 0.79, seccompiler (new dependency), libc (new direct dependency), cargo-nextest via `mise run`.

**Spec:** `docs/superpowers/specs/2026-10-08-unix-socket-isolation-design.md` — read it first.

## Global Constraints

- Work in a worktree `.claude/worktrees/unix-sockets`, branch `fix/unix-socket-isolation`, from `origin/main` (94914df or later).
- Commit messages and code comments are factual and describe the hardening; do **not** write an exploit walk-through. Say "sandboxed processes could connect to Unix sockets outside their grants, including the daemon's tmux server".
- Commit type `fix(runtime): …` / `fix(cli): …` / `fix(agent): …` (core patch release). No `feat:`.
- Config values exactly: `auto | mediate | deny | open`, default `auto`, key `[sandbox] unix_sockets` in the daemon's `config.toml`.
- Probe deadline exactly 10 s.
- seccomp: `socket(AF_UNIX, …)` fails with `EAFNOSUPPORT`; `socketpair` is allowed; filter installed after `PR_SET_NO_NEW_PRIVS`.
- No change to the public API of published crates `balerix-api` and `balerix-plugin-sdk` (0.2.1). This is why pod mode has no bundle field (see Task 6).
- New dependencies pinned the way `Cargo.toml` `[workspace.dependencies]` pins the others; `deny.toml` licences must pass (`mise run check` includes it).
- Gates per task: the task's own tests; before the PR: `mise run check`, `BALERIX_REQUIRE_TOOLS=1 mise run test-it`, `mise run agent`.

## Review Focus

1. A sandboxed process under `Deny` calls `socket(AF_UNIX)` via the x32 ABI (syscall number with bit 30 set) on x86_64 — must fail too (Task 2 test).
2. An agent's tools that use `socketpair` / pipes (node child processes, `git` subprocesses, `mise exec`) under `Deny` — must keep working (Task 2 and Task 7 tests).
3. A user/fleet/manifest `sandbox` block that sets `af_unix_mediation` at the top-level `linux` key, or in `platform_overrides.linux.linux`, or sets it to `off` explicitly — refused with the path named (Task 4 test).
4. The probe when nono is missing, hangs, or prints garbage — `auto` resolves to `Deny`, `mediate` refuses to start with the cause, never a panic or an `Open` (Task 1 and Task 3 tests).
5. A `sandbox-exec` invoked with an empty argv, or on a kernel/arch where the filter cannot be installed — exits non-zero with a message, never execs unfiltered (Task 2 test).

---

### Task 0: Verify the external facts (no code kept)

**Files:** none committed. Record findings at the bottom of this plan file under "Task 0 findings".

- [ ] **Step 1: nono schema keys.** In the worktree run `mise exec -- nono profile schema` into a schema dump and confirm, with the exact JSON paths:
  - top-level `linux.af_unix_mediation` with values `off` / `pathname`;
  - inside an OS override it is `platform_overrides.linux.linux.af_unix_mediation`;
  - `filesystem.unix_socket_subtree_bind` is a list of paths and (from nono docs / `nono run --help` / `nono why`) permits both bind and connect beneath the path under `pathname` mediation.
  If any differs, adjust Tasks 3 and 4 to the real keys before starting them.
- [ ] **Step 2: seccompiler.** `cargo search seccompiler --limit 1`; note the latest version and licence (expect Apache-2.0 OR BSD-3-Clause). Confirm `SeccompFilter`, `SeccompRule`, `SeccompCondition`, `SeccompCmpArgLen`, `SeccompCmpOp`, `SeccompAction`, `TargetArch`, `BpfProgram`, `apply_filter` exist in that version's docs (docs.rs). Confirm what `apply_filter` does about `PR_SET_NO_NEW_PRIVS` and what the arch-mismatch action is.
- [ ] **Step 3: host facts.** Confirm `cat /proc/sys/kernel/yama/ptrace_scope` prints `2` here (so Mediate tests will skip locally).
- [ ] **Step 4:** If Step 1 or 2 contradicts the plan, stop and report before Task 1.

### Task 1: `SocketPolicy`, the config key, and policy resolution

**Files:**
- Create: `crates/balerix-runtime/src/socket_policy.rs`
- Modify: `crates/balerix-runtime/src/lib.rs` (add `pub mod socket_policy;` and re-exports)
- Modify: `crates/balerix/src/commands/serve.rs:29-100` (`ServerConfig`, `SandboxTable`, `load_for`)

**Interfaces:**
- Produces:
  ```rust
  // balerix_runtime::socket_policy
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
  #[serde(rename_all = "lowercase")]
  pub enum UnixSockets { #[default] Auto, Mediate, Deny, Open }

  #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
  pub enum SocketPolicy { Mediate, Deny, #[default] Open }

  #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
  #[error("{0}")]
  pub struct ProbeFailure(pub String);

  /// The resolved policy and the one-line reason `serve` logs.
  pub fn resolve(
      setting: UnixSockets,
      probe: impl FnOnce() -> Result<(), ProbeFailure>,
  ) -> Result<(SocketPolicy, String), ProbeFailure>;
  ```
  `ServerConfig` gains `pub unix_sockets: UnixSockets`.
  `SocketPolicy::default()` is `Open` only so `Runtime::new` keeps today's behaviour for tests and `dev`; `serve` and the sidecar always set it explicitly.

- [ ] **Step 1: Write the failing tests** in `socket_policy.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn pass() -> Result<(), ProbeFailure> { Ok(()) }
    fn fail() -> Result<(), ProbeFailure> {
        Err(ProbeFailure("tcp connect refused under pathname mediation (ptrace_scope 2?)".into()))
    }

    #[test]
    fn auto_mediates_when_the_probe_passes() {
        let (p, why) = resolve(UnixSockets::Auto, pass).unwrap();
        assert_eq!(p, SocketPolicy::Mediate);
        assert!(why.contains("mediate"), "{why}");
    }

    #[test]
    fn auto_denies_when_the_probe_fails_and_says_why() {
        let (p, why) = resolve(UnixSockets::Auto, fail).unwrap();
        assert_eq!(p, SocketPolicy::Deny);
        assert!(why.contains("ptrace_scope"), "{why}");
    }

    #[test]
    fn mediate_refuses_to_start_when_the_probe_fails() {
        let e = resolve(UnixSockets::Mediate, fail).unwrap_err();
        assert!(e.0.contains("ptrace_scope"), "{e}");
    }

    #[test]
    fn deny_and_open_never_run_the_probe() {
        for (s, want) in [(UnixSockets::Deny, SocketPolicy::Deny), (UnixSockets::Open, SocketPolicy::Open)] {
            let (p, _) = resolve(s, || panic!("probe ran for {s:?}")).unwrap();
            assert_eq!(p, want);
        }
    }

    #[test]
    fn the_setting_parses_lowercase_and_refuses_others() {
        #[derive(serde::Deserialize)]
        struct T { v: UnixSockets }
        let p = |s: &str| toml::from_str::<T>(&format!("v = \"{s}\"")).map(|t| t.v);
        assert_eq!(p("auto").unwrap(), UnixSockets::Auto);
        assert_eq!(p("mediate").unwrap(), UnixSockets::Mediate);
        assert_eq!(p("deny").unwrap(), UnixSockets::Deny);
        assert_eq!(p("open").unwrap(), UnixSockets::Open);
        assert!(p("Deny").is_err() && p("off").is_err());
    }
}
```
  (If `toml` is not a dev-dependency of `balerix-runtime`, move the parse test to `serve.rs`'s tests instead and drop it here.)

- [ ] **Step 2: Run to see them fail.** `cargo nextest run -p balerix-runtime socket_policy` → compile error (module missing).
- [ ] **Step 3: Implement.**

```rust
//! Which Unix sockets a sandboxed process may use (spec 2026-10-08):
//! `Mediate` — nono's pathname mediation, own directories only;
//! `Deny` — no `socket(AF_UNIX)` at all, through `balerix sandbox-exec`;
//! `Open` — nono's default, any socket the uid can reach.

pub fn resolve(
    setting: UnixSockets,
    probe: impl FnOnce() -> Result<(), ProbeFailure>,
) -> Result<(SocketPolicy, String), ProbeFailure> {
    match setting {
        UnixSockets::Open => Ok((SocketPolicy::Open, "unix_sockets = \"open\": sandboxed processes can reach Unix sockets outside their grants, including the daemon's tmux server".into())),
        UnixSockets::Deny => Ok((SocketPolicy::Deny, "unix_sockets = \"deny\": sandboxed processes cannot create Unix sockets".into())),
        UnixSockets::Mediate => {
            probe()?;
            Ok((SocketPolicy::Mediate, "unix_sockets = \"mediate\": sandboxed processes use Unix sockets in their own directories only".into()))
        }
        UnixSockets::Auto => Ok(match probe() {
            Ok(()) => (SocketPolicy::Mediate, "unix_sockets = \"auto\": nono's pathname mediation works here; mediate: own directories only".into()),
            Err(e) => (SocketPolicy::Deny, format!("unix_sockets = \"auto\": nono's pathname mediation does not work here ({e}); deny: sandboxed processes cannot create Unix sockets (set \"open\" to allow them, with the risk)")),
        }),
    }
}
```
  In `serve.rs`: add `#[serde(default)] unix_sockets: UnixSockets` to `SandboxTable`, `pub unix_sockets: UnixSockets` to `ServerConfig`, copy it in `load_for`. Add a `serve.rs` test that `[sandbox]\nunix_sockets = "deny"` loads as `Deny`, an absent key as `Auto`, and `unix_sockets = "nope"` fails naming `config.toml`.
- [ ] **Step 4: Run.** `cargo nextest run -p balerix-runtime socket_policy` and `cargo nextest run -p balerix serve` → PASS.
- [ ] **Step 5: Commit** `fix(runtime): a Unix socket policy for sandboxed processes, resolved from [sandbox] unix_sockets`.

### Task 2: The seccomp filter and `balerix sandbox-exec`

**Files:**
- Modify: `Cargo.toml` (`[workspace.dependencies]`: `seccompiler`, `libc`), `crates/balerix-runtime/Cargo.toml` (both, `[target.'cfg(target_os = "linux")'.dependencies]`)
- Create: `crates/balerix-runtime/src/seccomp.rs` (Linux only: `#[cfg(target_os = "linux")]` in `lib.rs`)
- Modify: `crates/balerix/src/cli.rs:47-63` (new hidden `SandboxExec`), `crates/balerix/src/main.rs:18` (dispatch before tracing/runtime, like `AgentSupervise`)
- Create: `crates/balerix/tests/cli_sandbox_exec.rs`

**Interfaces:**
- Produces:
  ```rust
  // balerix_runtime::seccomp (linux only)
  /// Installs the filter on the calling thread (and so on everything it
  /// execs): `socket(AF_UNIX, …)` fails with EAFNOSUPPORT, including the
  /// x32 entry on x86_64; a foreign-arch syscall is fatal. Sets
  /// PR_SET_NO_NEW_PRIVS first.
  pub fn deny_unix_sockets() -> std::io::Result<()>;
  ```
  CLI: `balerix sandbox-exec -- <argv…>` → installs the filter, then `execvp(argv[0], argv)`; never returns on success; on any failure prints `balerix sandbox-exec: <cause>` to stderr and exits 126 (127 if argv[0] is not found).

- [ ] **Step 1: Write the failing CLI tests** in `crates/balerix/tests/cli_sandbox_exec.rs` (Linux only, `#![cfg(target_os = "linux")]`). Use `python3` from the host as the client (CI's `ubuntu-latest` has it; skip with a message if absent unless `BALERIX_REQUIRE_TOOLS=1`):

```rust
const BALERIX: &str = env!("CARGO_BIN_EXE_balerix");

fn under_filter(py: &str) -> std::process::Output {
    std::process::Command::new(BALERIX)
        .args(["sandbox-exec", "--", "python3", "-c", py])
        .output()
        .unwrap()
}

#[test]
fn a_unix_socket_cannot_be_created() {
    let out = under_filter(
        "import socket,errno\ntry:\n socket.socket(socket.AF_UNIX)\n print('created')\nexcept OSError as e:\n print(errno.errorcode[e.errno])",
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "EAFNOSUPPORT", "{out:?}");
}

#[test]
fn socketpair_tcp_and_children_still_work() {
    let out = under_filter(
        "import socket,subprocess\na,b=socket.socketpair()\na.send(b'x');assert b.recv(1)==b'x'\ns=socket.socket(socket.AF_INET);s.bind(('127.0.0.1',0));s.close()\nprint(subprocess.run(['true']).returncode)",
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0");
}

#[test]
fn the_filter_survives_exec_into_a_child() {
    // the child is a fresh python: it inherits the filter
    let out = under_filter(
        "import subprocess,sys\nr=subprocess.run([sys.executable,'-c','import socket\\ntry:\\n socket.socket(socket.AF_UNIX);print(1)\\nexcept OSError:\\n print(0)'],capture_output=True,text=True)\nprint(r.stdout.strip())",
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0", "{out:?}");
}

#[cfg(target_arch = "x86_64")]
#[test]
fn the_x32_entry_is_refused_too() {
    // syscall(__X32_SYSCALL_BIT | SYS_socket, AF_UNIX, SOCK_STREAM, 0)
    let out = under_filter(
        "import ctypes,os\nlibc=ctypes.CDLL(None,use_errno=True)\nr=libc.syscall(0x40000000|41,1,1,0)\nprint(r, ctypes.get_errno())",
    );
    let text = String::from_utf8_lossy(&out.stdout);
    // EAFNOSUPPORT (97) from the filter, or ENOSYS (38) from a kernel
    // without x32; never a descriptor
    assert!(text.starts_with("-1 97") || text.starts_with("-1 38"), "{text} {out:?}");
}

#[test]
fn an_empty_argv_is_an_error_not_an_unfiltered_exec() {
    let out = std::process::Command::new(BALERIX).args(["sandbox-exec", "--"]).output().unwrap();
    assert!(!out.status.success());
}
```
- [ ] **Step 2: Run.** `cargo nextest run -p balerix cli_sandbox_exec` → FAIL (no subcommand).
- [ ] **Step 3: Implement.** Add deps (versions from Task 0), e.g. `seccompiler = "<ver>"`, `libc = "0.2.<current lock version>"` in `[workspace.dependencies]`; in `balerix-runtime/Cargo.toml`:
  ```toml
  [target.'cfg(target_os = "linux")'.dependencies]
  seccompiler = { workspace = true }
  libc = { workspace = true }
  ```
  `seccomp.rs`:

```rust
//! `balerix sandbox-exec`'s filter (spec 2026-10-08 §3.3, policy Deny):
//! no `socket(AF_UNIX)`, so a sandboxed process cannot reach a Unix
//! socket outside its grants (nono leaves pathname sockets unmediated
//! by default). `socketpair` stays: pipes between a process and its
//! children are not a way out.
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};
use std::collections::BTreeMap;
use std::io;

#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: i64 = 0x4000_0000;

fn program() -> Result<BpfProgram, Box<dyn std::error::Error>> {
    let unix = || -> Result<Vec<SeccompRule>, seccompiler::BackendError> {
        Ok(vec![SeccompRule::new(vec![SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            libc::AF_UNIX as u64,
        )?])?])
    };
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    rules.insert(libc::SYS_socket, unix()?);
    #[cfg(target_arch = "x86_64")]
    rules.insert(X32_SYSCALL_BIT | libc::SYS_socket, unix()?);
    let arch: TargetArch = std::env::consts::ARCH.try_into()?;
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,                                  // no rule matched
        SeccompAction::Errno(libc::EAFNOSUPPORT as u32),       // AF_UNIX
        arch,
    )?;
    Ok(filter.try_into()?)
}

pub fn deny_unix_sockets() -> io::Result<()> {
    let prog = program().map_err(|e| io::Error::other(format!("cannot build the filter: {e}")))?;
    // seccompiler's apply_filter sets PR_SET_NO_NEW_PRIVS (confirm in Task 0;
    // if it does not, call libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) first)
    seccompiler::apply_filter(&prog)
        .map_err(|e| io::Error::other(format!("cannot install the filter: {e}")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_program_builds_for_this_arch() {
        assert!(!super::program().unwrap().is_empty());
    }
}
```
  If seccompiler's matching of the x32 number fails (it may validate syscall numbers), drop that rule and instead add a second rule that errors any syscall with `X32_SYSCALL_BIT` via a hand-written check — or, simpler, note that x86_64 kernels on CI/the development host are built with `CONFIG_X86_X32_ABI` off (the test accepts ENOSYS). Report which.

  `cli.rs`, next to `AgentSupervise`:
```rust
    /// Runs a command with Unix socket creation refused (seccomp), for
    /// `[sandbox] unix_sockets = "deny"`. Profiles put it inside nono.
    #[command(hide = true)]
    SandboxExec {
        #[arg(last = true, required = true, value_name = "ARGV")]
        argv: Vec<std::ffi::OsString>,
    },
```
  `main.rs`, before anything else (mirror the `AgentSupervise` early return):
```rust
    if let Command::SandboxExec { argv } = &cli.command {
        return commands::sandbox::exec(argv);
    }
```
  Create `crates/balerix/src/commands/sandbox.rs`:
```rust
//! `sandbox-exec` (and, in Task 3, `sandbox-probe`).
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::ExitCode;

pub fn exec(argv: &[OsString]) -> ExitCode {
    let Some((program, args)) = argv.split_first() else {
        eprintln!("balerix sandbox-exec: no command");
        return ExitCode::from(126);
    };
    #[cfg(target_os = "linux")]
    if let Err(e) = balerix_runtime::seccomp::deny_unix_sockets() {
        eprintln!("balerix sandbox-exec: {e}");
        return ExitCode::from(126);
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("balerix sandbox-exec: Linux only");
        return ExitCode::from(126);
    }
    let e = std::process::Command::new(program).args(args).exec();
    eprintln!("balerix sandbox-exec: {}: {e}", program.to_string_lossy());
    ExitCode::from(if e.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 })
}
```
  (`clap`'s `required = true` already rejects an empty argv with exit 2; keep the `split_first` guard anyway.)
- [ ] **Step 4: Run.** `cargo nextest run -p balerix cli_sandbox_exec` and `cargo nextest run -p balerix-runtime seccomp` → PASS. `mise run check` lint step (clippy, deny) → PASS.
- [ ] **Step 5: Commit** `fix(cli): balerix sandbox-exec refuses Unix socket creation for the command it runs`.

### Task 3: `balerix sandbox-probe` and the mediation probe

**Files:**
- Modify: `crates/balerix/src/cli.rs` (hidden `SandboxProbe`), `crates/balerix/src/main.rs`, `crates/balerix/src/commands/sandbox.rs`
- Modify: `crates/balerix-runtime/src/socket_policy.rs` (add `probe_mediation`)
- Create: `crates/balerix/tests/cli_sandbox_probe.rs`

**Interfaces:**
- Consumes: `ProbeFailure` (Task 1), `ToolPaths` (`crates/balerix-runtime/src/tools.rs`: `.nono`, `.balerix`).
- Produces:
  ```rust
  // balerix_runtime::socket_policy
  /// Runs `balerix sandbox-probe` under a minimal nono profile with
  /// pathname mediation; Ok when all three checks hold. `scratch` is a
  /// directory the caller owns (created if absent, emptied afterwards).
  pub fn probe_mediation(tools: &ToolPaths, scratch: &Path) -> Result<(), ProbeFailure>;
  ```
  CLI: `balerix sandbox-probe --tcp <port> --inside <dir> --outside <socket path>` prints exactly one JSON line `{"tcp":"ok"|"<error>","inside":"ok"|"<error>","outside":"refused"|"connected"|"<error>"}` and exits 0 iff tcp=="ok" && inside=="ok" && outside=="refused".

- [ ] **Step 1: Write the failing tests** in `cli_sandbox_probe.rs` — run the probe outside any sandbox (so: tcp ok, inside ok, outside **connected** → exit 1) and under `sandbox-exec` (inside fails with EAFNOSUPPORT → exit 1):

```rust
#![cfg(target_os = "linux")]
const BALERIX: &str = env!("CARGO_BIN_EXE_balerix");

struct Fixture { _dir: tempfile::TempDir, tcp: std::net::TcpListener, inside: std::path::PathBuf, outside: std::path::PathBuf, _ul: std::os::unix::net::UnixListener }

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let inside = dir.path().join("inside");
    std::fs::create_dir(&inside).unwrap();
    let outside = dir.path().join("outside.sock");
    let ul = std::os::unix::net::UnixListener::bind(&outside).unwrap();
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    Fixture { _dir: dir, tcp, inside, outside, _ul: ul }
}

fn probe(f: &Fixture, prefix: &[&str]) -> (bool, serde_json::Value) {
    let port = f.tcp.local_addr().unwrap().port().to_string();
    let mut cmd = std::process::Command::new(prefix.first().copied().unwrap_or(BALERIX));
    cmd.args(prefix.iter().skip(1));
    if !prefix.is_empty() { cmd.arg(BALERIX); }
    let out = cmd
        .args(["sandbox-probe", "--tcp", &port, "--inside"]).arg(&f.inside)
        .arg("--outside").arg(&f.outside)
        .output().unwrap();
    let line = String::from_utf8_lossy(&out.stdout);
    (out.status.success(), serde_json::from_str(line.trim()).unwrap_or_else(|e| panic!("{e}: {line:?} {out:?}")))
}

#[test]
fn unsandboxed_the_outside_socket_connects_so_the_probe_fails() {
    let f = fixture();
    let (ok, v) = probe(&f, &[]);
    assert_eq!(v, serde_json::json!({"tcp":"ok","inside":"ok","outside":"connected"}));
    assert!(!ok);
}

#[test]
fn under_sandbox_exec_the_inside_socket_fails_so_the_probe_fails() {
    let f = fixture();
    let (ok, v) = probe(&f, &[BALERIX, "sandbox-exec", "--"]);
    assert_eq!(v["tcp"], "ok");
    assert!(v["inside"].as_str().unwrap().contains("not supported") || v["inside"].as_str().unwrap().contains("97"), "{v}");
    assert!(!ok);
}
```
  And in `crates/balerix-runtime` add a test (in `tests/sandbox_it.rs`, which already gates on nono via its existing helper — reuse that helper and its `BALERIX_REQUIRE_TOOLS` behaviour) — the probe's real outcome depends on the host:

```rust
#[test]
fn the_mediation_probe_reports_this_hosts_answer() {
    let Some(tools) = /* the file's existing tools-or-skip helper */ else { return };
    let scratch = tempfile::tempdir().unwrap();
    let got = balerix_runtime::socket_policy::probe_mediation(&tools, scratch.path());
    let ptrace = std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope").unwrap_or_default();
    if std::env::var_os("BALERIX_REQUIRE_MEDIATION").is_some_and(|v| v == "1") {
        assert_eq!(got, Ok(()), "BALERIX_REQUIRE_MEDIATION=1 but the probe failed (ptrace_scope {ptrace})");
    } else if ptrace.trim() == "2" {
        assert!(got.is_err(), "ptrace_scope 2 should make pathname mediation unusable: {got:?}");
    }
    assert!(std::fs::read_dir(scratch.path()).unwrap().next().is_none(), "scratch left behind");
}
```
  `probe_mediation` needs the balerix binary: in `balerix-runtime` tests `ToolPaths.balerix` may point at a binary that lacks `sandbox-probe` until rebuilt — follow how `sandbox_it`/`materialize_it` resolve `balerix` today (search for `CARGO_BIN_EXE` / `tools.balerix` in `crates/balerix-runtime/tests/support`); if they use a stand-in, put this test in `crates/balerix/tests/cli_sandbox_probe.rs` instead, building `ToolPaths` with `balerix = BALERIX`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.** `commands/sandbox.rs`:

```rust
pub fn probe(tcp: u16, inside: &Path, outside: &Path) -> ExitCode {
    use std::os::unix::net::{UnixListener, UnixStream};
    let show = |r: std::io::Result<()>| match r { Ok(()) => "ok".to_string(), Err(e) => e.to_string() };
    let tcp_r = show(std::net::TcpStream::connect(("127.0.0.1", tcp)).map(drop));
    let sock = inside.join("probe.sock");
    let inside_r = show((|| {
        let l = UnixListener::bind(&sock)?;
        let _c = UnixStream::connect(&sock)?;
        drop(l);
        std::fs::remove_file(&sock)
    })());
    let outside_r = match UnixStream::connect(outside) {
        Ok(_) => "connected".to_string(),
        Err(e) if matches!(e.raw_os_error(), Some(libc_eperm) if libc_eperm == 1 || libc_eperm == 13) => "refused".to_string(),
        Err(e) => e.to_string(),
    };
    let pass = tcp_r == "ok" && inside_r == "ok" && outside_r == "refused";
    println!("{}", serde_json::json!({"tcp": tcp_r, "inside": inside_r, "outside": outside_r}));
    if pass { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
```
  (EPERM = 1, EACCES = 13: write them as named constants; `balerix` may not depend on `libc` — use the numbers with a comment, or `rustix::io::Errno`.)

  `socket_policy::probe_mediation`:
  1. `create_dir_all(scratch)`; `inside = scratch/inside` (created), `outside_dir = scratch/outside` (created), `outside = outside_dir/probe.sock`, bind a `UnixListener` there; bind `TcpListener` on `127.0.0.1:0`; spawn a thread that `accept()`s on both listeners until the probe ends (so connects complete).
  2. Write `scratch/profile.json`:
     ```json
     {"meta":{"name":"balerix-socket-probe"},
      "filesystem":{"read":[...SYSTEM_READ, "<canonical tools.balerix>"],
                    "unix_socket_subtree_bind":["<inside>"]},
      "workdir":{"access":"none"},
      "network":{"open_port":[<tcp port>]},
      "environment":{"deny_vars":["*"],"set_vars":{"PATH":"/usr/bin:/bin"}},
      "linux":{"af_unix_mediation":"pathname"}}
     ```
     (`inside` also needs write access for `bind` to create the socket file — add it to `filesystem.allow`.)
  3. Run `Cmd::new(&tools.nono)` (the crate's `crate::cmd::Cmd`, as `host_landlock_abi` does) with `env_clear()`, `HOME=scratch`, `NONO_NO_UPDATE_CHECK=1`, args `["-s","run","--no-audit","--profile",<profile>,"--",<balerix>,"sandbox-probe","--tcp",<port>,"--inside",<inside>,"--outside",<outside>]`, with a **10 s** deadline. If `Cmd` has no timeout, spawn `std::process::Command`, poll `try_wait` every 50 ms up to 10 s, kill on expiry → `ProbeFailure("no answer within 10 s")`.
  4. Exit 0 → `Ok(())`. Otherwise → `ProbeFailure(format!("{json line or stderr first line}; kernel.yama.ptrace_scope = {n}"))`, reading `/proc/sys/kernel/yama/ptrace_scope` when present.
  5. Always remove `scratch`'s contents afterwards (listener, socket, profile).
  On non-Linux, `probe_mediation` returns `Err(ProbeFailure("pathname mediation is Linux only"))` — not used on macOS (Task 6).
- [ ] **Step 4: Run** the new tests → PASS (here the runtime probe test passes through its `ptrace_scope == 2` branch).
- [ ] **Step 5: Commit** `fix(runtime): probe whether nono's pathname mediation works on this host`.

### Task 4: Applying the policy to profiles, and pinning it

**Files:**
- Modify: `crates/balerix-runtime/src/socket_policy.rs` (add `apply_to_profile`)
- Modify: `crates/balerix-runtime/src/sandbox.rs:309-410` (`check_conflicts`, `check_security` neighbour)
- Test: unit tests in both files

**Interfaces:**
- Consumes: `SocketPolicy` (Task 1).
- Produces:
  ```rust
  /// The rendered profile under `policy`. Mediate: pathname mediation at
  /// the top level and in platform_overrides.linux.linux, and
  /// unix_socket_subtree_bind gains `own` (kept after any operator
  /// entries). Deny: `wrapper` (the balerix binary) is added to
  /// filesystem.read so `sandbox-exec` can run. Open: unchanged.
  pub fn apply_to_profile(profile: Value, policy: SocketPolicy, own: &[PathBuf], wrapper: &Path) -> Value;
  ```

- [ ] **Step 1: Write the failing tests.**

```rust
#[test]
fn mediate_pins_pathname_mediation_and_grants_own_dirs() {
    let base = json!({"filesystem":{"read":["/usr"],"unix_socket_subtree_bind":["/run/op.sock.d"]},
                      "platform_overrides":{"linux":{"security":{"signal_mode":"isolated"}}}});
    let p = apply_to_profile(base, SocketPolicy::Mediate, &[PathBuf::from("/w"), PathBuf::from("/h")], Path::new("/opt/balerix"));
    assert_eq!(p["linux"]["af_unix_mediation"], "pathname");
    assert_eq!(p["platform_overrides"]["linux"]["linux"]["af_unix_mediation"], "pathname");
    assert_eq!(p["platform_overrides"]["linux"]["security"]["signal_mode"], "isolated", "kept");
    assert_eq!(p["filesystem"]["unix_socket_subtree_bind"], json!(["/run/op.sock.d", "/w", "/h"]));
    assert_eq!(p["filesystem"]["read"], json!(["/usr"]), "no wrapper under Mediate");
}

#[test]
fn deny_adds_the_wrapper_to_read_once_and_nothing_else() {
    let base = json!({"filesystem":{"read":["/usr","/opt/balerix"]}});
    let p = apply_to_profile(base.clone(), SocketPolicy::Deny, &[PathBuf::from("/w")], Path::new("/opt/balerix"));
    assert_eq!(p["filesystem"]["read"], json!(["/usr","/opt/balerix"]));
    assert!(p.get("linux").is_none());
    let p = apply_to_profile(json!({"filesystem":{"read":["/usr"]}}), SocketPolicy::Deny, &[], Path::new("/opt/balerix"));
    assert_eq!(p["filesystem"]["read"], json!(["/usr","/opt/balerix"]));
}

#[test]
fn open_changes_nothing() {
    let base = json!({"filesystem":{"read":["/usr"]}});
    assert_eq!(apply_to_profile(base.clone(), SocketPolicy::Open, &[PathBuf::from("/w")], Path::new("/b")), base);
}
```
  In `sandbox.rs` tests, beside `signal_mode_is_balerix_owned_at_every_level`:
```rust
#[test]
fn af_unix_mediation_is_balerix_owned_at_every_level() {
    let (id, grants, env) = fixture();
    let refuse = |user: Value| render_profile(&id, &grants, 1, &env, &user).unwrap_err().to_string();
    assert_eq!(
        refuse(json!({ "linux": { "af_unix_mediation": "off" } })),
        "f/c/a: sandbox.linux.af_unix_mediation: balerix-owned; set [sandbox] unix_sockets in the daemon's config.toml"
    );
    assert_eq!(
        refuse(json!({ "linux": { "af_unix_mediation": null } })),
        "f/c/a: sandbox.linux.af_unix_mediation: balerix-owned; set [sandbox] unix_sockets in the daemon's config.toml"
    );
    assert_eq!(
        refuse(json!({ "platform_overrides": { "linux": { "linux": { "af_unix_mediation": "pathname" } } } })),
        "f/c/a: sandbox.platform_overrides.linux.linux.af_unix_mediation: balerix-owned; set [sandbox] unix_sockets in the daemon's config.toml"
    );
    // other `linux` keys are not this check's business
    render_profile(&id, &grants, 1, &env, &json!({ "linux": {} })).unwrap();
}
```
  (Match the exact error formatting of the existing `signal_mode` test — copy its `refuse` helper and message shape; adjust the expected strings to that format if it differs.)
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.** In `check_conflicts`, after `check_security(u, "", &conflict)?`, call `check_linux(u, "", &conflict)?`; in the `platform_overrides` loop also `check_linux(patch, &format!("platform_overrides.{name}."), &conflict)?`:

```rust
/// `linux.af_unix_mediation` is balerix's (spec 2026-10-08 §3.4): the
/// daemon's `[sandbox] unix_sockets` decides it for every sandbox.
fn check_linux(
    block: &serde_json::Map<String, Value>,
    prefix: &str,
    conflict: &impl Fn(String, String) -> MaterializeError,
) -> Result<(), MaterializeError> {
    if let Some(Value::Object(linux)) = block.get("linux")
        && linux.contains_key("af_unix_mediation")
    {
        return Err(conflict(
            format!("{prefix}linux.af_unix_mediation"),
            "balerix-owned; set [sandbox] unix_sockets in the daemon's config.toml".into(),
        ));
    }
    Ok(())
}
```
  (Match `check_security`'s exact signature for `conflict`.) `apply_to_profile` in `socket_policy.rs` — straightforward `serde_json` edits: ensure objects exist with `as_object_mut()`/`entry().or_insert(json!({}))`, push `own` entries not already present, push `wrapper` to `filesystem.read` if absent.
- [ ] **Step 4: Run** `cargo nextest run -p balerix-runtime sandbox:: socket_policy` → PASS; existing golden snapshots unchanged (policy not applied yet).
- [ ] **Step 5: Commit** `fix(runtime): pin linux.af_unix_mediation to balerix and apply the socket policy to a profile`.

### Task 5: Carrying the policy through the runtime

**Files:**
- Modify: `crates/balerix-runtime/src/materializer.rs:20-60` (`Runtime.socket_policy`, `with_socket_policy`), `:90-130` (agent profile + launch), the plugin path in `crates/balerix-runtime/src/plugin.rs:255-300` and `render_plugin_launch` `:119-160`, `crates/balerix-runtime/src/launch.rs:36-100` (`render_launch`), `crates/balerix-runtime/src/workspace.rs` (`Workspace` struct ~`:641`, `sandbox_args` `:354`, `sandboxed_cmd` `:386`, `sandbox_starts` `:941`, the shell-script call around `:1258`, and the git profile writer `write_git_profile` in `sandbox.rs:~225`)
- Test: golden files in `crates/balerix-runtime/tests/generated_golden.rs`, `plugin_golden.rs` (new cases per policy; existing snapshots stay = Open)

**Interfaces:**
- Consumes: `SocketPolicy`, `apply_to_profile` (Tasks 1, 4).
- Produces:
  ```rust
  impl Runtime { #[must_use] pub fn with_socket_policy(self, policy: SocketPolicy) -> Self; }
  // socket_policy.rs
  /// argv after nono's `--`: under Deny, prefixed with `<balerix> sandbox-exec --`.
  pub fn wrap_command(policy: SocketPolicy, balerix: &Path, argv: Vec<String>) -> Vec<String>;
  ```
  `render_launch(paths, tools, binary, args, resume, policy: SocketPolicy)` and `render_plugin_launch(plugin, paths, tools, policy: SocketPolicy)` gain the last parameter. `write_git_profile(..., git_read, policy)` gains it too. `Workspace` gains `pub socket_policy: SocketPolicy`.

- [ ] **Step 1: Write the failing tests.**
  - `socket_policy.rs`: `wrap_command(Deny, "/b", ["mise","exec","--","claude"])` == `["/b","sandbox-exec","--","mise","exec","--","claude"]`; `Mediate` and `Open` return the argv unchanged.
  - `launch.rs` tests: `render_launch(..., SocketPolicy::Deny)` argv contains, right after nono's `--`, `[tools.balerix, "sandbox-exec", "--", tools.mise, "exec", "--", binary]`, and the script text contains `'sandbox-exec'`; `Open` is byte-identical to today's (the existing launch test keeps passing with `SocketPolicy::Open`).
  - `plugin.rs` tests: same for `render_plugin_launch`.
  - Golden: add `deny` and `mediate` variants of one agent and one plugin profile + launch to `generated_golden.rs` / `plugin_golden.rs` (`cargo insta` snapshots). Under Mediate the agent's `unix_socket_subtree_bind` is `[home, workspace]`, the plugin's `[home, scratch]`; the git profile under Mediate gets the `linux` key and **no** `unix_socket_subtree_bind` (pass `own = &[]`).
  - `workspace.rs` unit test: `sandbox_args(&agent, SocketPolicy::Deny, Path::new("/b"))` ends with `["--", "/b", "sandbox-exec", "--"]`.
- [ ] **Step 2: Run** → FAIL (signatures).
- [ ] **Step 3: Implement.**
  - `Runtime { …, pub socket_policy: SocketPolicy }`, default `Open` in `new`, `with_socket_policy` builder like `with_git_read`.
  - Agent render (`materializer.rs` ~`:105`): `let profile = apply_to_profile(render_profile(...)?, self.socket_policy, &[paths.home.clone(), paths.workspace.clone()], &self.tools.balerix);`. Under `Deny`, if `agent.settings.sandbox` has any `filesystem.unix_socket*` key and `profile_changed`, `tracing::warn!(agent = %id, "the fleet's sandbox grants Unix sockets, which unix_sockets = \"deny\" makes unusable")`.
  - Plugin render (`plugin.rs` ~`:279`): same with `&[paths.home.clone(), paths.scratch.clone()]`.
  - `render_launch`: build the post-`--` part (`mise exec -- binary args…`) as a `Vec`, pass it through `wrap_command(policy, &tools.balerix, …)`, then extend `argv`. Update the caller in `materializer.rs` to pass `self.socket_policy`, and every test call.
  - `render_plugin_launch`: same around `mise run <start>`.
  - Git: `write_git_profile` applies `apply_to_profile(profile, policy, &[], &tools.balerix)`; `sandbox_args(agent, policy, balerix)` appends the wrapper after `--`; thread `self.socket_policy` from `Workspace` (set it where `Workspace { tools, git_read, … }` is built in `materializer.rs` `:263`, `:389`, `:416`). Check `sandbox_starts` and the script at `~:1258` and route both through `sandbox_args` or add the wrapper the same way. `grep -n "tools.nono" crates/balerix-runtime/src/*.rs` must show no nono invocation left that skips the policy, except `host_landlock_abi`, `sandbox_self_test` and `probe_mediation`.
- [ ] **Step 4: Run** `cargo nextest run -p balerix-runtime` and `cargo insta test -p balerix-runtime --review` (accept only the new snapshots; existing ones must be unchanged) → PASS.
- [ ] **Step 5: Commit** `fix(runtime): render agents', plugins' and the daemon's git sandboxes under the socket policy`.

### Task 6: Resolving the policy in `serve` and in the pod sidecar

**Files:**
- Modify: `crates/balerix/src/commands/serve.rs:370-390` (start-up), `:450-465` (beside `warn_without_signal_scoping`)
- Modify: `agent/src/sidecar.rs:160-170`
- Test: `crates/balerix/tests/cli_serve.rs` (one test), `serve.rs` unit test

**Interfaces:**
- Consumes: `resolve`, `probe_mediation`, `with_socket_policy`.
- Produces: `fn socket_policy(config: &ServerConfig, tools: &ToolPaths, paths: &ServerPaths) -> anyhow::Result<SocketPolicy>` in `serve.rs`.

- [ ] **Step 1: Write the failing tests.**
  - `cli_serve.rs`: start `serve` with `config.toml` `[sandbox]\nunix_sockets = "deny"` (follow the file's existing start helper), wait for the endpoint, and assert the log contains `unix_sockets = "deny"`. Second case: `unix_sockets = "mediate"` with a stand-in nono so the probe fails on any host → `serve` exits non-zero and stderr names the probe's cause.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.**
```rust
/// Spec 2026-10-08 §3.1: once per start; the probe runs in the server
/// directory's `socket-probe/`.
fn socket_policy(config: &ServerConfig, tools: &ToolPaths, paths: &ServerPaths) -> Result<SocketPolicy> {
    if !cfg!(target_os = "linux") {
        tracing::info!("unix_sockets: not applied on this OS (macOS check pending)");
        return Ok(SocketPolicy::Open);
    }
    let scratch = paths.dir.join("socket-probe");
    let (policy, why) = balerix_runtime::socket_policy::resolve(config.unix_sockets, || {
        balerix_runtime::socket_policy::probe_mediation(tools, &scratch)
    })
    .map_err(|e| anyhow!("config.toml: sandbox.unix_sockets = \"mediate\": {e}"))?;
    match policy {
        SocketPolicy::Open => tracing::warn!("{why}"),
        _ => tracing::info!("{why}"),
    }
    Ok(policy)
}
```
  Call it right after `warn_without_signal_scoping(&tools, paths);` and before the runtime is built: `Runtime::new(...).with_git_read(...).with_socket_policy(policy)`.
  Sidecar: before `Runtime::new` at `sidecar.rs:164`, resolve with `UnixSockets::Auto` and `probe_mediation(&tools, &args.run_dir.join("socket-probe"))` inside `spawn_blocking`; log the reason; `Runtime::new(...).with_socket_policy(policy)`. No bundle field: `AgentBundle` is in published `balerix-api` with `deny_unknown_fields`, so a new field would break older sidecars and the crate's API. (Deviation from spec §3.5, reported to the user; an operator override is a follow-up.)
- [ ] **Step 4: Run** `cargo nextest run -p balerix serve cli_serve` and `mise run agent` → PASS.
- [ ] **Step 5: Commit** `fix(runtime): resolve the Unix socket policy when serve and the pod sidecar start`.

### Task 7: End-to-end tests against a real tmux server, and CI

**Files:**
- Create: `crates/balerix/tests/cli_sandbox_sockets.rs`
- Modify: `.github/workflows/ci.yml` (the `test-it`/check job that runs these tests: env `BALERIX_REQUIRE_MEDIATION: "1"` and a step printing `cat /proc/sys/kernel/yama/ptrace_scope`)

- [ ] **Step 1: Write the tests** (gate on nono + tmux like `cli_supervise.rs:329`): build a profile with `balerix_runtime::render_profile` for a temp `AgentPaths` (or a minimal hand-written one mirroring the agent baseline: SYSTEM_READ + the balerix binary + a temp writable dir, `open_port`), start a private tmux server `tmux -S <tmp>/t.sock new-session -d` outside every grant, and run checks from inside `nono run --profile <p> -- [balerix sandbox-exec --] …`:
  - `open_policy_control_can_connect_to_the_tmux_server` — no wrapper, policy Open: `balerix sandbox-probe --outside <tmp>/t.sock …` reports `outside: "connected"`. (Proves the fixture can see the socket; if it cannot, the test setup is wrong.)
  - `deny_policy_cannot_connect_to_a_tmux_server_outside_its_grants` — with `apply_to_profile(.., Deny, ..)` and the wrapper: the read-only `tmux -S <tmp>/t.sock has-session` exits non-zero and its stderr names the cause, "Address family not supported".
  - `mediate_policy_own_sockets_work_and_cannot_connect_to_a_tmux_server_outside_its_grants` — only when `probe_mediation` passes (else skip with a message; with `BALERIX_REQUIRE_MEDIATION=1` panic instead): profile via `apply_to_profile(.., Mediate, &[own], ..)`; `has-session` exits non-zero with a non-empty stderr; and `balerix sandbox-probe --inside <own> …` inside the sandbox reports `tcp: ok`, `inside: ok`, `outside: refused`.
  - `deny_policy_claude_like_child_processes_work` — inside the Deny sandbox run `sh -c 'printf x | cat'` and `python3 -c "import subprocess;subprocess.run(['true'],check=True)"` (skip python if absent) → success.
  Kill the tmux server in a `Drop` guard.
- [ ] **Step 2: Run** `BALERIX_REQUIRE_TOOLS=1 cargo nextest run -p balerix cli_sandbox_sockets` → Open control passes, Deny passes, Mediate skips here.
- [ ] **Step 3: CI.** Add to the job that runs `test-it` / `cli_*` integration tests:
```yaml
      - run: cat /proc/sys/kernel/yama/ptrace_scope || echo unavailable
```
  and `BALERIX_REQUIRE_MEDIATION: "1"` in that job's `env`. If the PR's CI shows the probe failing on the runner, remove the env, keep the print, and report — the Mediate path then has no CI coverage and the user decides what to do.
- [ ] **Step 4: Commit** `test: Unix socket policy end to end against a real tmux server`.

### Task 8: Docs, upgrade note, and the spec and plan copies

**Files:**
- Modify: `AGENTS.md` (gotcha), `README.md` (the `config.toml` `[sandbox]` section that documents `git_read`), `docs/THREAT-MODEL.md` (new row + agent/plugin sandbox rows), `CHANGELOG.md` (`### Upgrading` under `# Changelog`, as #24 did)
- Create (copied): `docs/superpowers/specs/2026-10-08-unix-socket-isolation-design.md`, `docs/superpowers/plans/2026-10-08-unix-socket-isolation.md` (from the working copies)

- [ ] **Step 1: README** — under `[sandbox]`: `unix_sockets = "auto"` with the four values in one table (what each does, what breaks under `deny`: local postgres/docker/`git fsmonitor` in an agent's workspace fail with "address family not supported").
- [ ] **Step 2: AGENTS.md** gotcha: `linux.af_unix_mediation` is balerix-owned and refused in every `sandbox` block; on hosts where nono's pathname mediation does not work (`kernel.yama.ptrace_scope = 2`, restricted containers) `auto` resolves to `deny`; the probe; `sandbox-exec`; tests `cli_sandbox_*` and `BALERIX_REQUIRE_MEDIATION`.
- [ ] **Step 3: THREAT-MODEL** — a row: "A sandboxed process connecting to a Unix socket outside its grants (the daemon's tmux server, ssh-agent)" → controls (§3.1–3.4 in one paragraph), limits (macOS unverified; `open` reopens it; agents launched before the upgrade until their next launch; pod mode always `auto`). Keep it factual, no exploit steps.
- [ ] **Step 4: CHANGELOG** `### Upgrading`: new `[sandbox] unix_sockets` (default `auto`); on restricted hosts agents can no longer create Unix sockets unless `open`; restart agents (`balerix down`/`up` or a daemon restart re-renders on the first pass) so they pick up the new profile.
- [ ] **Step 5:** Copy the spec and this plan into `docs/superpowers/{specs,plans}/` (append "Task 0 findings" and any deviations to the plan copy).
- [ ] **Step 6: Gates** — `mise run check`, `BALERIX_REQUIRE_TOOLS=1 mise run test-it`, `mise run agent` → all PASS.
- [ ] **Step 7: Commit** `docs: Unix socket policy, upgrade note, spec and plan`.
- [ ] **Step 8 (after merge, by hand):** `mise run verify-claude` on the development host (policy Deny) and one real agent session doing a commit and a push; confirm claude and its tools work.

## Known deviations from the spec (to confirm with the user)

1. **Pod mode has no operator setting** (spec §3.5 said a bundle field): `AgentBundle` is in published `balerix-api` with `deny_unknown_fields`; the sidecar always resolves `auto`. Override is a follow-up.
2. **Foreign-arch syscalls kill the process** (seccompiler's arch check) rather than failing with an errno; x32 `socket` is refused explicitly (or is ENOSYS where the kernel lacks x32).
3. **The fleet `unix_socket*` warning under Deny** is logged when an agent's profile changes, not once per fleet.

## Task 0 findings

### Findings (2026-10-08)

**nono 0.79.0 schema** (`mise exec -- nono profile schema`, saved to a schema dump). All keys match the plan.
- `linux.af_unix_mediation`: `$defs/LinuxConfig/properties/af_unix_mediation`, `$ref LinuxAfUnixMediation` = enum `["off","pathname"]`, nullable. Description: 'pathname' requires explicit `filesystem.unix_socket*` grants for pathname Unix socket connect/bind.
- In an OS override: `platform_overrides.linux` is a `$ref: "#"` (full profile; `extends`/`platform_overrides` nested are parse errors), so the key is `platform_overrides.linux.linux.af_unix_mediation`. Confirmed.
- `filesystem.unix_socket_subtree_bind`: array of ConditionalPath. Schema description: "Directories where any descendant AF_UNIX socket may be connected to or bound. Recursive. Implies read+write access." (sibling `unix_socket_subtree`, connect only, implies read.) Confirmed from the schema text (not run-tested; ptrace_scope=2 blocks mediation here).
- Also present: `linux.sandbox_policy` (auto default = Landlock + static seccomp network baseline).

**seccompiler**: latest 0.5.0, licence `Apache-2.0 OR BSD-3-Clause` (cargo info). Source read from the registry copy.
- Re-exported at crate root: SeccompFilter, SeccompRule, SeccompCondition, SeccompCmpArgLen (Dword|Qword), SeccompCmpOp, SeccompAction (Allow, Errno(u32), KillThread, KillProcess, Log, Trace, Trap), TargetArch, BpfProgram, BpfProgramRef, sock_filter; `apply_filter`, `apply_filter_all_threads` are crate-level fns. All exist.
- `SeccompFilter::new(rules: BTreeMap<i64, Vec<SeccompRule>>, mismatch_action, match_action, target_arch) -> Result`; only validation is mismatch != match action. `SeccompCondition::new(arg_index: u8, len, op, value: u64)`, `SeccompRule::new(Vec<SeccompCondition>)`. Convert with `BpfProgram::try_from(filter)` (verified).
- Semantics: mismatch_action applies to syscalls NOT in the map; match_action to rules that match.
- `apply_filter` DOES call `prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0)` itself (lib.rs ~344) before seccomp(2); errors on empty program (`EmptyFilter`). So NO_NEW_PRIVS is set implicitly; the explicit prctl in the plan is redundant but harmless (and "installed after NO_NEW_PRIVS" holds either way). Applies to calling thread only; `apply_filter_all_threads` uses TSYNC.
- Arch mismatch: the program begins with an arch check; mismatch returns `SECCOMP_RET_KILL_PROCESS`, hard-coded and not configurable (bpf.rs build_arch_validation_sequence).
- x32 key: `new()` accepts `0x4000_0000|41` as an i64 key (no range validation; compile does `i64.try_into::<u32>()`, which fits). Verified by a scratch program: map with keys 41 and 0x40000041 -> `SeccompFilter::new` ok, conversion ok (19 insns). The arch check uses AUDIT_ARCH_X86_64, which x32 syscalls also pass, so x32 socket(0x40000029) is NOT caught by a key of 41; the extra key is needed. Note the compare is against seccomp_data.nr, which includes the x32 bit, so it works as intended.

**Host**: `/proc/sys/kernel/yama/ptrace_scope` = 2 (Mediate tests skip locally). `libc` in Cargo.lock = 0.2.189. Host arch x86_64.

**Contradictions with the plan**: none. Only notes: (a) apply_filter sets NO_NEW_PRIVS itself; (b) arch mismatch is fixed KILL_PROCESS.

## Deviations during execution

- Under `deny`, `sandbox-exec` also fails `io_uring_setup`, `io_uring_enter` and `io_uring_register` with `ENOSYS` (io_uring can create sockets without `socket()`); the two errnos need two stacked seccomp filters.
- The probe profile has no `environment.set_vars` (nono reserves `PATH`); the probe passes only if its JSON line is exactly `{"tcp":"ok","inside":"ok","outside":"refused"}`.
- The pod sidecar always resolves `auto` (no bundle field: `AgentBundle` is in published `balerix-api` with `deny_unknown_fields`). When it resolves `deny`, it checks at start-up that the image's `balerix` supports `sandbox-exec` and refuses to start otherwise, so agent images must be built on a balerix base of this release or later (also in the CHANGELOG Upgrading note).
- The operator's pod harvest Job runs sandboxed git with policy `open` (waived: the Job pod has no tmux server, agent sockets or daemon token); listed in THREAT-MODEL limits.
- Fleet `unix_socket*` grants under `deny`: the warning is logged when an agent's profile changes, not once per fleet.
- Foreign-architecture syscalls kill the process (seccompiler's architecture check); x32 `socket` and io_uring are refused explicitly.
