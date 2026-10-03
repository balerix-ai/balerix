//! Shared scaffolding for the agent project's integration tests: the real
//! tools (skipped, or failed under `BALERIX_REQUIRE_TOOLS=1`), temp roots
//! under `target/tmp`, and the `balerix` binary `scripts/agent.sh` built.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

pub mod fake_daemon;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct Tools {
    pub tmux: PathBuf,
    pub git: PathBuf,
    /// The sidecar discovers its tools with `ToolPaths::discover_in`, which
    /// requires `gh` too.
    pub gh: PathBuf,
    pub mise: PathBuf,
    pub nono: PathBuf,
    /// From `BALERIX_BIN` (`scripts/agent.sh check` sets it).
    pub balerix: PathBuf,
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Every tool, or `None` after printing a skip (a failure under
/// `BALERIX_REQUIRE_TOOLS=1`).
pub fn tools() -> Option<Tools> {
    let balerix = std::env::var_os("BALERIX_BIN").map(PathBuf::from);
    let found = (|| {
        Some(Tools {
            tmux: on_path("tmux")?,
            git: on_path("git")?,
            gh: on_path("gh")?,
            mise: on_path("mise")?,
            nono: on_path("nono")?,
            balerix: balerix.filter(|p| p.is_file())?,
        })
    })();
    if found.is_none() {
        assert!(!require_or_skip(
            "tmux+git+gh+mise+nono on PATH and BALERIX_BIN (run through scripts/agent.sh)",
            false
        ));
    }
    found
}

/// Prints `skip: <name> missing` and returns false, or panics when
/// `BALERIX_REQUIRE_TOOLS=1`.
pub fn require_or_skip(name: &str, present: bool) -> bool {
    if present {
        return true;
    }
    if std::env::var("BALERIX_REQUIRE_TOOLS").as_deref() == Ok("1") {
        panic!("{name} missing and BALERIX_REQUIRE_TOOLS=1");
    }
    eprintln!("skip: {name} missing");
    false
}

/// `<target/tmp>/<label>-<pid>`, created fresh. Never `/tmp`: nono grants
/// it by default.
pub fn temp_root(label: &str) -> PathBuf {
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// True when Landlock is usable: `nono run` of `true` succeeds. The same
/// probe `balerix-runtime`'s tests use (crates/balerix-runtime/tests/support):
/// `--allow-cwd` because nono refuses CWD access non-interactively without
/// it, and `$HOME` a sibling of `root`, not inside it, since nono refuses a
/// broad allow over its per-file denials under `$HOME` ("Landlock
/// deny-overlap is not enforceable").
pub fn landlock_works(tools: &Tools, root: &Path) -> bool {
    let home = root.with_file_name(format!(
        "{}-nono-probe-home",
        root.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::create_dir_all(&home).unwrap();
    let result = std::process::Command::new(&tools.nono)
        .args(["-s", "run", "--allow-cwd", "--", "/bin/true"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .current_dir(root)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = std::fs::remove_dir_all(&home);
    result
}

pub fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Ends the tmux server on `socket`, if one is there. A test's server is
/// a daemon, outlives the `balerix-agent run` that started it once that is
/// SIGKILLed, and keeps the panes' processes alive; every guard that holds
/// one calls this. stderr is nulled: a server already gone prints nothing.
pub fn kill_tmux_server(tmux: &Path, socket: &Path) {
    let _ = std::process::Command::new(tmux)
        .arg("-S")
        .arg(socket)
        .args(["-u", "kill-server"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Every socket under `root` a tmux server still answers on.
pub fn live_tmux_servers(tmux: &Path, root: &Path) -> Vec<PathBuf> {
    use std::os::unix::fs::FileTypeExt;
    let mut live = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(kind) = e.file_type() else { continue };
            if kind.is_dir() {
                dirs.push(e.path());
            } else if kind.is_socket()
                && std::process::Command::new(tmux)
                    .arg("-S")
                    .arg(e.path())
                    .args(["-u", "list-sessions"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success())
            {
                live.push(e.path());
            }
        }
    }
    live
}

/// Kills the tmux server on `socket` when dropped, the test failed or not.
pub struct TmuxServer {
    pub tmux: PathBuf,
    pub socket: PathBuf,
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        kill_tmux_server(&self.tmux, &self.socket);
    }
}
