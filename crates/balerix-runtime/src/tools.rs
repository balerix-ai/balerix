//! Tool discovery and a logged subprocess runner. Argv arrays only.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    cwd: Option<PathBuf>,
    log: Option<PathBuf>,
    log_stdout: bool,
    timeout: Option<std::time::Duration>,
}

#[derive(Debug)]
pub(crate) struct CmdOutput {
    pub stdout: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CmdFailure {
    pub tool: String,
    pub subcommand: String,
    pub args: Vec<String>,
    pub stderr: String,
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
    /// Kills the child and fails if it outlives `d`. Only the daemon pool
    /// install sets this today (Spec F §6); every other call is unbounded as
    /// before.
    pub(crate) fn timeout(mut self, d: std::time::Duration) -> Self {
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
        let mut c = Command::new(&self.program);
        c.args(&self.args).envs(&self.env);
        for k in &self.env_removals {
            c.env_remove(k);
        }
        if let Some(d) = &self.cwd {
            c.current_dir(d);
        }
        let failure = |stderr: String| CmdFailure {
            tool: self.tool(),
            subcommand: self.subcommand(),
            args: self.args.clone(),
            stderr,
        };
        let cannot_execute =
            |e: std::io::Error| failure(format!("cannot execute {}: {e}", self.program.display()));
        let out = match stdin {
            None => match self.timeout {
                None => c.output().map_err(cannot_execute)?,
                Some(d) => {
                    let mut child = c
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .map_err(cannot_execute)?;
                    // Only the pipe handles move to the drain threads; the
                    // parent keeps the `Child` so it can poll and, on
                    // expiry, kill it directly (safe: no PATH lookup, no
                    // `unsafe`). Draining stdout and stderr on their own
                    // threads is what keeps a chatty child from filling a
                    // pipe buffer and deadlocking us while we poll.
                    let (mut stdout_pipe, mut stderr_pipe) =
                        match (child.stdout.take(), child.stderr.take()) {
                            (Some(o), Some(e)) => (o, e),
                            _ => {
                                return Err(failure(
                                    "cannot capture output: stdout/stderr pipe missing after spawn"
                                        .to_string(),
                                ));
                            }
                        };
                    let stdout_thread = std::thread::spawn(move || {
                        let mut buf = Vec::new();
                        let _ = stdout_pipe.read_to_end(&mut buf);
                        buf
                    });
                    let stderr_thread = std::thread::spawn(move || {
                        let mut buf = Vec::new();
                        let _ = stderr_pipe.read_to_end(&mut buf);
                        buf
                    });
                    let deadline = std::time::Instant::now() + d;
                    let status = loop {
                        match child.try_wait().map_err(cannot_execute)? {
                            Some(status) => break Some(status),
                            None if std::time::Instant::now() >= deadline => break None,
                            None => std::thread::sleep(std::time::Duration::from_millis(20)),
                        }
                    };
                    match status {
                        Some(status) => std::process::Output {
                            status,
                            stdout: stdout_thread.join().unwrap_or_default(),
                            stderr: stderr_thread.join().unwrap_or_default(),
                        },
                        None => {
                            let _ = child.kill();
                            let _ = child.wait();
                            return Err(failure(format!("timed out after {}s", d.as_secs())));
                        }
                    }
                }
            },
            Some(input) => {
                let mut child = c
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(cannot_execute)?;
                // Dropped (and so closed) before the wait: the child sees EOF.
                let written = match child.stdin.take() {
                    Some(mut pipe) => pipe.write_all(input),
                    None => Ok(()),
                };
                if let Err(e) = written {
                    // Do not wait on a child that may never read its input.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(failure(format!(
                        "cannot write to {}: {e}",
                        self.program.display()
                    )));
                }
                child.wait_with_output().map_err(cannot_execute)?
            }
        };
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if let Some(log) = &self.log {
            if let Some(dir) = log.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log) {
                let logged_stdout = if self.log_stdout { stdout.as_str() } else { "" };
                let _ = writeln!(
                    f,
                    "$ {} {}\n{logged_stdout}{stderr}[exit {}]",
                    self.tool(),
                    self.args.join(" "),
                    out.status
                );
            }
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(logged.contains("[exit exit status: 3]"));
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
        assert!(logged.contains("[exit exit status: 0]"));
    }

    #[test]
    fn an_accepted_exit_code_is_success_and_keeps_stdout() {
        let sh = Cmd::new(Path::new("/bin/sh")).args(["-c", "echo out; exit 1"]);
        assert!(sh.run().is_err());
        assert_eq!(sh.run_with_exit_codes(&[0, 1]).unwrap().stdout, "out\n");
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
