//! Tool discovery and a logged subprocess runner. Argv arrays only.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

/// The stderr a run keeps: its last 64 KiB (`StderrTail`). One sandbox
/// start of the workspace reader runs up to 500 git commands, and their
/// stderr is the only output not bounded by its consumer.
pub(crate) const STDERR_LIMIT: usize = 64 * 1024;

/// How long a timed-out child's tree gets between SIGTERM and SIGKILL
/// (`supervise::terminate_tree`). SIGTERM first because a wrapper such
/// as `nono` forwards it to what it runs, and SIGKILL to the wrapper alone
/// would leave that running.
pub(crate) const TIMEOUT_GRACE: Duration = Duration::from_secs(2);

/// How long a timed-out run waits for the tree's stderr to reach its end,
/// for the log.
const STDERR_AFTER_TIMEOUT: Duration = Duration::from_millis(200);

/// The last `cap` bytes of a stream, read as it arrives, and how many
/// bytes before them were dropped.
#[derive(Debug)]
pub(crate) struct StderrTail {
    buf: Vec<u8>,
    dropped: u64,
    cap: usize,
}

impl StderrTail {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            buf: Vec::new(),
            dropped: 0,
            cap,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        // only the end of a chunk larger than the cap can survive it
        let bytes = if bytes.len() > self.cap {
            let skip = bytes.len() - self.cap;
            self.dropped += skip as u64;
            &bytes[skip..]
        } else {
            bytes
        };
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > self.cap {
            let excess = self.buf.len() - self.cap;
            self.dropped += excess as u64;
            self.buf.drain(..excess);
        }
    }

    /// Reads `r` to its end (or first error), keeping the tail.
    fn read(mut r: impl Read, cap: usize) -> Self {
        let mut tail = Self::new(cap);
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            match r.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => tail.push(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        tail
    }

    /// The kept bytes as text, after a line that says how many were
    /// dropped when any were.
    pub(crate) fn into_string(self) -> String {
        let kept = String::from_utf8_lossy(&self.buf);
        if self.dropped == 0 {
            kept.into_owned()
        } else {
            format!("[… {} bytes of stderr dropped]\n{kept}", self.dropped)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPaths {
    pub git: PathBuf,
    pub gh: PathBuf,
    pub mise: PathBuf,
    pub nono: PathBuf,
    pub tmux: PathBuf,
    /// This binary: the `SessionStart` relay hook runs it inside the
    /// sandbox (Phase 3 spec P3-3), so the profile grants it read-only.
    pub balerix: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("required tool not found on PATH: {0}")]
pub struct MissingTool(pub String);

impl ToolPaths {
    /// Finds each tool as the first executable file on `path`. `balerix` is
    /// this binary's own path (the `SessionStart` relay hook target), not
    /// discovered on `PATH`.
    pub fn discover_in(path: &OsStr, balerix: &Path) -> Result<Self, MissingTool> {
        let find = |name: &str| -> Result<PathBuf, MissingTool> {
            std::env::split_paths(path)
                .map(|d| d.join(name))
                .find(|p| p.is_file())
                .ok_or_else(|| MissingTool(name.to_string()))
        };
        Ok(Self {
            git: find("git")?,
            gh: find("gh")?,
            mise: find("mise")?,
            nono: find("nono")?,
            tmux: find("tmux")?,
            balerix: balerix.to_path_buf(),
        })
    }
}

/// One subprocess invocation.
pub(crate) struct Cmd {
    program: PathBuf,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    env_removals: Vec<String>,
    env_clear: bool,
    cwd: Option<PathBuf>,
    log: Option<PathBuf>,
    log_stdout: bool,
    timeout: Option<std::time::Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CmdOutput {
    pub stdout: String,
    /// What the child printed on stderr. A caller that accepts a non-zero
    /// exit as an answer reads it to tell the tool's silent "no" from a
    /// wrapper's failure with the same code (`Workspace::agent_git`).
    pub stderr: String,
    /// The exit code, one of the accepted ones: how a caller that accepts
    /// `[0, 1]` tells a "no" (1) from a "yes" (0).
    pub code: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CmdFailure {
    pub tool: String,
    pub subcommand: String,
    pub args: Vec<String>,
    pub stderr: String,
    /// Set when the run was ended by its `timeout`, which this was.
    pub timed_out: Option<Duration>,
}

impl fmt::Display for CmdFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.tool,
            self.subcommand,
            balerix_core::first_line(&self.stderr)
        )
    }
}

impl Cmd {
    pub(crate) fn new(program: &Path) -> Self {
        Self {
            program: program.to_path_buf(),
            args: Vec::new(),
            env: BTreeMap::new(),
            env_removals: Vec::new(),
            env_clear: false,
            cwd: None,
            log: None,
            log_stdout: true,
            timeout: None,
        }
    }
    pub(crate) fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, a: I) -> Self {
        self.args.extend(a.into_iter().map(Into::into));
        self
    }
    pub(crate) fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }
    pub(crate) fn envs(mut self, vars: &BTreeMap<String, String>) -> Self {
        self.env
            .extend(vars.iter().map(|(k, v)| (k.clone(), v.clone())));
        self
    }
    /// Removes `k` from the child's environment, clearing any value this
    /// process inherited for it. Removals are applied in `run` after every
    /// `env`/`envs` call on this `Cmd`, so a removed key stays removed even
    /// if `env`/`envs` for it was called first.
    pub(crate) fn env_remove(mut self, k: impl Into<String>) -> Self {
        self.env_removals.push(k.into());
        self
    }
    /// Starts the child from an empty environment: only what `env`/`envs`
    /// set on this `Cmd` reaches it. For a `nono run` the daemon makes on
    /// its own behalf, where an inherited `NONO_ALLOW` would widen the
    /// sandbox.
    pub(crate) fn env_clear(mut self) -> Self {
        self.env_clear = true;
        self
    }
    pub(crate) fn cwd(mut self, d: &Path) -> Self {
        self.cwd = Some(d.to_path_buf());
        self
    }
    /// Append `$ argv`, stdout and stderr to this file after the run.
    pub(crate) fn log(mut self, file: &Path) -> Self {
        self.log = Some(file.to_path_buf());
        self.log_stdout = true;
        self
    }
    /// `log` without the stdout: `$ argv`, stderr and the exit status
    /// only. For a command whose output is the payload of a request
    /// rather than a trace worth keeping — `inspect.rs` runs one
    /// `diff -U3` per file per page load, and logging those would grow the
    /// crew's `git.log` by the whole diff on every fetch.
    pub(crate) fn log_argv_only(mut self, file: &Path) -> Self {
        self.log = Some(file.to_path_buf());
        self.log_stdout = false;
        self
    }
    /// Fails the run if the child outlives `d`, after ending the child and
    /// every process below it (`spawn_and_wait`). Set by the daemon pool
    /// install (Spec F §6), the sandbox probes and the workspace reader's
    /// sandbox starts; every other call is unbounded.
    pub(crate) fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }
    pub(crate) fn tool(&self) -> String {
        self.program
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
    /// First argument that does not start with `-`, e.g. `clone` in
    /// `git -C x clone`. If every argument is a flag (or this value is
    /// consumed by one, e.g. `-c` in `sh -c '…'`), falls back to the first
    /// argument, so `sh -c '…'` reports its subcommand as `-c`.
    pub(crate) fn subcommand(&self) -> String {
        let mut skip_value = false;
        for a in &self.args {
            if skip_value {
                skip_value = false;
                continue;
            }
            if a == "-C" || a == "-c" || a == "-L" || a == "-t" {
                skip_value = true;
                continue;
            }
            if !a.starts_with('-') {
                return a.clone();
            }
        }
        self.args.first().cloned().unwrap_or_default()
    }

    pub(crate) fn run(&self) -> Result<CmdOutput, CmdFailure> {
        self.exec(&[0], None)
    }

    /// `run`, treating any exit code in `accepted` as success: `git diff
    /// --no-index` exits 1 when the files differ, which is the answer.
    pub(crate) fn run_with_exit_codes(&self, accepted: &[i32]) -> Result<CmdOutput, CmdFailure> {
        self.exec(accepted, None)
    }

    /// `run`, feeding `input` on the child's stdin and closing it: how a
    /// payload larger than an argv reaches a tool (`tmux load-buffer -`).
    /// The write is synchronous and the pipe buffer is 64 KiB, so this
    /// suits a tool that consumes stdin before it writes much of its own
    /// output — which `load-buffer` does (it writes none).
    pub(crate) fn run_with_stdin(&self, input: &[u8]) -> Result<CmdOutput, CmdFailure> {
        self.exec(&[0], Some(input))
    }

    fn exec(&self, accepted: &[i32], stdin: Option<&[u8]>) -> Result<CmdOutput, CmdFailure> {
        let mut stdout = Vec::new();
        let (status, stderr) = self.spawn_and_wait(stdin, &mut |chunk| {
            stdout.extend_from_slice(chunk);
        })?;
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        self.finish(accepted, status, stdout, stderr)
    }

    /// `run_with_exit_codes`, handing stdout to `sink` in chunks as it
    /// arrives instead of collecting it: for output only its consumer can
    /// bound (`inspect.rs`'s combined diff keeps 256 KiB per file of a
    /// diff that may be far larger). `CmdOutput::stdout` is empty, and
    /// the log never carries stdout. The `timeout`, if set, holds here too.
    pub(crate) fn run_streaming(
        &self,
        accepted: &[i32],
        sink: &mut dyn FnMut(&[u8]),
    ) -> Result<CmdOutput, CmdFailure> {
        let (status, stderr) = self.spawn_and_wait(None, sink)?;
        self.finish(accepted, status, String::new(), stderr)
    }

    fn failure(&self, stderr: String) -> CmdFailure {
        CmdFailure {
            tool: self.tool(),
            subcommand: self.subcommand(),
            args: self.args.clone(),
            stderr,
            timed_out: None,
        }
    }

    /// Runs the child to its exit: `stdin` written and closed first (no
    /// stdin at all without it), stdout handed to `on_stdout` on this
    /// thread as it arrives, stderr kept to its last `STDERR_LIMIT` bytes
    /// (`StderrTail`). stdout and stderr are each drained on a thread of
    /// their own, which is what keeps a chatty child from filling a pipe
    /// and deadlocking the wait. Past the `timeout`, the child's whole tree
    /// is ended (`supervise::terminate_tree`: SIGTERM, `TIMEOUT_GRACE`,
    /// then SIGKILL to every descendant and the child) and the run fails
    /// with `timed_out` set; the drain threads are not waited for then.
    fn spawn_and_wait(
        &self,
        stdin: Option<&[u8]>,
        on_stdout: &mut dyn FnMut(&[u8]),
    ) -> Result<(std::process::ExitStatus, String), CmdFailure> {
        let mut c = Command::new(&self.program);
        c.args(&self.args);
        if self.env_clear {
            c.env_clear();
        }
        c.envs(&self.env);
        for k in &self.env_removals {
            c.env_remove(k);
        }
        if let Some(d) = &self.cwd {
            c.current_dir(d);
        }
        let mut child = c
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| self.failure(format!("cannot execute {}: {e}", self.program.display())))?;
        let started = Instant::now();
        if let Some(input) = stdin {
            // Dropped (and so closed) before the wait: the child sees EOF.
            // The write is synchronous and the pipe buffer is 64 KiB (see
            // `run_with_stdin`).
            let written = match child.stdin.take() {
                Some(mut pipe) => pipe.write_all(input),
                None => Ok(()),
            };
            if let Err(e) = written {
                // Do not wait on a child that may never read its input.
                let _ = child.kill();
                let _ = child.wait();
                return Err(
                    self.failure(format!("cannot write to {}: {e}", self.program.display()))
                );
            }
        }
        let (Some(mut stdout_pipe), Some(stderr_pipe)) = (child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(self.failure(
                "cannot capture output: stdout/stderr pipe missing after spawn".to_string(),
            ));
        };
        // a channel, not a join: after a timeout this waits for the tail
        // only briefly, in case something outside the tree holds the pipe
        let (stderr_tx, stderr_rx) = std::sync::mpsc::sync_channel::<String>(1);
        std::thread::spawn(move || {
            let _ = stderr_tx.send(StderrTail::read(stderr_pipe, STDERR_LIMIT).into_string());
        });
        // Bounded, so a sink slower than the child holds the child back
        // rather than this process's memory.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);
        std::thread::spawn(move || {
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                match stdout_pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(chunk[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });
        let deadline = self.timeout.map(|d| (started + d, d));
        let timed_out = |child: &mut std::process::Child, d: Duration| {
            crate::supervise::terminate_tree(child, TIMEOUT_GRACE);
            // the tree is gone, so its pipe has closed: what it said is
            // the log's, not the caller's
            let said = stderr_rx
                .recv_timeout(STDERR_AFTER_TIMEOUT)
                .unwrap_or_default();
            self.write_log("", &said, &format_args!("timed out after {d:?}"));
            CmdFailure {
                timed_out: Some(d),
                ..self.failure(format!("timed out after {d:?}"))
            }
        };
        // stdout first, to its end
        loop {
            let chunk = match deadline {
                None => rx.recv().map_err(|_| ()),
                Some((at, d)) => {
                    match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                        Ok(chunk) => Ok(chunk),
                        Err(RecvTimeoutError::Disconnected) => Err(()),
                        Err(RecvTimeoutError::Timeout) => return Err(timed_out(&mut child, d)),
                    }
                }
            };
            match chunk {
                Ok(chunk) => on_stdout(&chunk),
                Err(()) => break,
            }
        }
        // then the exit
        let status = match deadline {
            None => child.wait(),
            Some((at, d)) => loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) if Instant::now() >= at => return Err(timed_out(&mut child, d)),
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                    Err(e) => break Err(e),
                }
            },
        }
        .map_err(|e| self.failure(format!("cannot wait for {}: {e}", self.program.display())))?;
        let stderr = stderr_rx.recv().unwrap_or_default();
        Ok((status, stderr))
    }

    /// The log entry: `$ argv`, stdout (unless `log_argv_only`), stderr
    /// and `[outcome]`.
    fn write_log(&self, stdout: &str, stderr: &str, outcome: &dyn fmt::Display) {
        let Some(log) = &self.log else {
            return;
        };
        if let Some(dir) = log.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log) {
            let logged_stdout = if self.log_stdout { stdout } else { "" };
            let _ = writeln!(
                f,
                "$ {} {}\n{logged_stdout}{stderr}[{outcome}]",
                self.tool(),
                self.args.join(" "),
            );
        }
    }

    /// The log line and the exit-code check every run ends with.
    fn finish(
        &self,
        accepted: &[i32],
        status: std::process::ExitStatus,
        stdout: String,
        stderr: String,
    ) -> Result<CmdOutput, CmdFailure> {
        self.write_log(&stdout, &stderr, &status);
        let code = status.code().filter(|c| accepted.contains(c));
        let Some(code) = code else {
            return Err(self.failure(if stderr.trim().is_empty() {
                // `ExitStatus` displays as `exit status: 3` (or `signal: 9`)
                status.to_string()
            } else {
                stderr
            }));
        };
        Ok(CmdOutput {
            stdout,
            stderr,
            code,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sandboxed git call starts from nothing the daemon inherited
    /// (`NONO_ALLOW`, `GIT_DIR`, …): only what the call sets.
    #[test]
    fn env_clear_drops_the_inherited_environment() {
        let out = Cmd::new(Path::new("/usr/bin/env"))
            .env_clear()
            .env("ONLY", "this")
            .run()
            .unwrap();
        assert_eq!(out.stdout, "ONLY=this\n");
    }

    #[test]
    fn discover_finds_tools_on_path_and_names_the_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        for t in ["git", "gh", "mise", "nono"] {
            std::fs::write(dir.path().join(t), "").unwrap();
        }
        let err =
            ToolPaths::discover_in(dir.path().as_os_str(), Path::new("/opt/balerix")).unwrap_err();
        assert_eq!(err.to_string(), "required tool not found on PATH: tmux");
        std::fs::write(dir.path().join("tmux"), "").unwrap();
        let t = ToolPaths::discover_in(dir.path().as_os_str(), Path::new("/opt/balerix")).unwrap();
        assert_eq!(t.tmux, dir.path().join("tmux"));
        assert_eq!(t.balerix, PathBuf::from("/opt/balerix"));
    }

    #[test]
    fn subcommand_skips_flags_with_values() {
        let c =
            Cmd::new(Path::new("/usr/bin/git")).args(["-C", "/x", "-c", "k=v", "worktree", "add"]);
        assert_eq!(c.subcommand(), "worktree");
        assert_eq!(
            Cmd::new(Path::new("/t/tmux"))
                .args(["-L", "s", "new-window"])
                .subcommand(),
            "new-window"
        );
        assert_eq!(c.tool(), "git");
    }

    #[test]
    fn run_captures_output_logs_and_fails_on_nonzero() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs").join("sh.log");
        let ok = Cmd::new(Path::new("/bin/sh"))
            .args(["-c", "echo out; echo err >&2"])
            .log(&log)
            .run()
            .unwrap();
        assert_eq!(ok.stdout, "out\n");
        let err = Cmd::new(Path::new("/bin/sh"))
            .args(["-c", "echo bad >&2; exit 3"])
            .log(&log)
            .run()
            .unwrap_err();
        assert_eq!(err.stderr, "bad\n");
        assert_eq!(err.to_string(), "sh -c: bad");
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains("$ sh -c echo out"));
        assert!(logged.contains("err\n"), "stderr is logged even on success");
        assert!(logged.contains("[exit status: 3]"), "{logged}");
        // with nothing on stderr, the status is the message, said once
        let err = Cmd::new(Path::new("/bin/sh"))
            .args(["-c", "exit 127"])
            .run()
            .unwrap_err();
        assert_eq!(err.stderr, "exit status: 127");
    }

    /// `log_argv_only` keeps the command and its exit status (and stderr,
    /// which a failure needs) but never the stdout: `inspect.rs` logs one
    /// `diff -U3` per file per page load, and the full diffs would grow
    /// `git.log` by megabytes a fetch.
    #[test]
    fn an_argv_only_log_records_the_command_and_exit_but_no_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs").join("git.log");
        // The marker is upper-cased by the command, so it appears in the
        // stdout but not in the argv the log does keep.
        let out = Cmd::new(Path::new("/bin/sh"))
            .args(["-c", "echo the-diff | tr a-z A-Z; echo loud >&2"])
            .log_argv_only(&log)
            .run()
            .unwrap();
        assert_eq!(out.stdout, "THE-DIFF\n", "the caller still gets stdout");
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(
            !logged.contains("THE-DIFF"),
            "stdout must not be logged: {logged:?}"
        );
        assert!(logged.contains("$ sh -c echo the-diff | tr a-z A-Z; echo loud >&2"));
        assert!(logged.contains("loud\n"), "stderr is still logged");
        assert!(logged.contains("[exit status: 0]"), "{logged}");
    }

    #[test]
    fn an_accepted_exit_code_is_success_and_keeps_stdout() {
        let sh = Cmd::new(Path::new("/bin/sh")).args(["-c", "echo out; exit 1"]);
        assert!(sh.run().is_err());
        let out = sh.run_with_exit_codes(&[0, 1]).unwrap();
        assert_eq!(out.stdout, "out\n");
        assert_eq!(out.code, 1, "the accepted code is reported");
        assert!(sh.run_with_exit_codes(&[2]).is_err());
    }

    #[test]
    fn env_remove_clears_an_inherited_or_just_set_variable() {
        let out = Cmd::new(Path::new("/bin/sh"))
            .args(["-c", "echo \"${GIT_DIR:-unset}\""])
            .env("GIT_DIR", "/x")
            .env_remove("GIT_DIR")
            .run()
            .unwrap();
        assert_eq!(out.stdout, "unset\n");
    }

    #[test]
    fn run_reports_a_missing_program() {
        let err = Cmd::new(Path::new("/nonexistent/tool"))
            .args(["x"])
            .run()
            .unwrap_err();
        assert!(err.stderr.starts_with("cannot execute /nonexistent/tool"));
    }

    #[test]
    fn a_timeout_kills_a_hung_child_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs").join("sleep.log");
        let pidfile = dir.path().join("pid");
        let start = std::time::Instant::now();
        let err = Cmd::new(Path::new("/bin/sh"))
            .args(["-c", &format!("echo $$ > {}; sleep 30", pidfile.display())])
            .log(&log)
            .timeout(std::time::Duration::from_secs(2))
            .run()
            .expect_err("a 30s sleep under a 2s timeout must fail");
        assert!(
            err.stderr.contains("timed out"),
            "expected a timeout message, got {:?}",
            err.stderr
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "the timeout must not wait out the child"
        );

        // The assertions above only check what `Cmd` *reports*; they still
        // pass if `child.kill()` is deleted. Prove the child process itself
        // is actually dead, not just that we stopped waiting on it
        // (commit 87f7309 fixed exactly this gap). Linux-only (`/proc`) is
        // fine: this crate is already Landlock-bound.
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap_or_else(|e| panic!("child never wrote its pid to {pidfile:?}: {e}"))
            .trim()
            .parse()
            .expect("pidfile did not contain a pid");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            // A killed child that `Cmd` has also `wait()`-ed on (which it
            // does right after `kill()`) is fully reaped: no `/proc` entry
            // at all, not even a zombie. Checking for the directory's
            // absence — rather than, say, just not erroring on a read — is
            // what actually distinguishes "killed and reaped" from "still
            // running" or "zombie, not yet reaped".
            if !Path::new(&format!("/proc/{pid}")).exists() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "pid {pid} is still present under /proc 5s after the timeout should have killed it"
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Reads the pid a test's shell wrote to `file`.
    fn pid_in(file: &Path) -> u32 {
        std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("no pid in {file:?}: {e}"))
            .trim()
            .parse()
            .expect("not a pid")
    }

    /// `pid` has exited (a zombie counts: re-parented to pid 1, it waits
    /// for that to reap it), within 5 s.
    fn assert_gone(pid: u32) {
        let proc = crate::supervise::Procfs::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while proc.identity(pid).unwrap().is_some() {
            assert!(
                Instant::now() < deadline,
                "pid {pid} still runs 5 s after the timeout ended its tree"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// #213: a deadline ends the whole tree, not just the child. The shell
    /// dies of the SIGTERM and its `sleep` is re-parented away from it; a
    /// kill of the child alone would leave that `sleep` running.
    #[test]
    fn a_timeout_ends_the_grandchildren_too() {
        let dir = tempfile::tempdir().unwrap();
        let (sh_pid, sleep_pid) = (dir.path().join("sh"), dir.path().join("sleep"));
        let err = Cmd::new(Path::new("/bin/sh"))
            .args([
                "-c",
                &format!(
                    "echo $$ > {}; sleep 30 & echo $! > {}; wait",
                    sh_pid.display(),
                    sleep_pid.display()
                ),
            ])
            .timeout(Duration::from_secs(2))
            .run()
            .expect_err("a 30 s sleep under a 2 s timeout must fail");
        assert_eq!(err.timed_out, Some(Duration::from_secs(2)));
        assert_eq!(err.stderr, "timed out after 2s");
        assert_gone(pid_in(&sh_pid));
        assert_gone(pid_in(&sleep_pid));
    }

    /// A tree that ignores SIGTERM gets `TIMEOUT_GRACE`, then SIGKILL, and
    /// the streaming run is bounded the same way.
    #[test]
    fn a_tree_that_ignores_sigterm_is_killed_after_the_grace() {
        let dir = tempfile::tempdir().unwrap();
        let (sh_pid, sleep_pid) = (dir.path().join("sh"), dir.path().join("sleep"));
        let start = Instant::now();
        let mut seen = Vec::new();
        let err = Cmd::new(Path::new("/bin/sh"))
            .args([
                "-c",
                &format!(
                    "trap '' TERM; echo $$ > {}; sleep 30 & echo $! > {}; echo hi; wait",
                    sh_pid.display(),
                    sleep_pid.display()
                ),
            ])
            .timeout(Duration::from_secs(2))
            .run_streaming(&[0], &mut |chunk| seen.extend_from_slice(chunk))
            .expect_err("a 30 s sleep under a 2 s timeout must fail");
        assert!(err.timed_out.is_some(), "{err:?}");
        assert_eq!(seen, b"hi\n", "stdout up to the deadline reached the sink");
        let took = start.elapsed();
        assert!(
            took >= Duration::from_secs(2) + TIMEOUT_GRACE && took < Duration::from_secs(30),
            "SIGTERM is ignored, so the grace runs out: {took:?}"
        );
        assert_gone(pid_in(&sh_pid));
        assert_gone(pid_in(&sleep_pid));
    }

    /// A timed-out run's log says so, as a finished one's says its status.
    #[test]
    fn a_timeout_is_logged() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs").join("git.log");
        Cmd::new(Path::new("/bin/sh"))
            .args(["-c", "echo said >&2; sleep 30"])
            .log_argv_only(&log)
            .timeout(Duration::from_secs(2))
            .run()
            .unwrap_err();
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(
            logged.contains("$ sh -c echo said >&2; sleep 30\nsaid\n[timed out after 2s]"),
            "the partial stderr is logged: {logged}"
        );
    }

    /// #213: stderr keeps its last `STDERR_LIMIT` bytes, after a line that
    /// says how many went, on every path: a failure, a success, a run
    /// under a timeout and a streamed one.
    #[test]
    fn stderr_is_capped_to_its_tail() {
        let flood = "head -c 200000 /dev/zero | tr '\\0' a >&2; echo END >&2";
        let check = |stderr: &str| {
            let dropped = 200_000 + 4 - STDERR_LIMIT;
            let (marker, kept) = stderr.split_once('\n').unwrap();
            assert_eq!(marker, format!("[… {dropped} bytes of stderr dropped]"));
            assert_eq!(kept.len(), STDERR_LIMIT);
            assert!(kept.ends_with("aaaaEND\n"), "the tail is kept");
        };
        let sh = |script: String| Cmd::new(Path::new("/bin/sh")).args(["-c".to_string(), script]);
        let err = sh(format!("{flood}; exit 3")).run().unwrap_err();
        check(&err.stderr);
        assert_eq!(
            err.to_string(),
            format!(
                "sh -c: [… {} bytes of stderr dropped]",
                200_004 - STDERR_LIMIT
            )
        );
        check(&sh(flood.to_string()).run().unwrap().stderr);
        check(
            &sh(flood.to_string())
                .timeout(Duration::from_secs(60))
                .run()
                .unwrap()
                .stderr,
        );
        check(
            &sh(flood.to_string())
                .run_streaming(&[0], &mut |_| {})
                .unwrap()
                .stderr,
        );
        // under the cap, stderr is as it was
        assert_eq!(sh("echo e >&2".into()).run().unwrap().stderr, "e\n");
    }

    #[test]
    fn the_tail_keeps_the_last_bytes_whatever_the_chunks() {
        let mut tail = StderrTail::new(4);
        tail.push(b"ab");
        tail.push(b"cdef");
        assert_eq!(tail.dropped, 2);
        tail.push(b"0123456789");
        assert_eq!(tail.into_string(), "[… 12 bytes of stderr dropped]\n6789");
        let mut small = StderrTail::new(4);
        small.push(b"abcd");
        assert_eq!(small.into_string(), "abcd");
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
}
