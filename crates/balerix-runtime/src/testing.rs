//! Scratch roots for integration tests, and the reaping of what earlier
//! runs left behind. Every test root is `<tmp>/<prefix>-<pid>`; the pid
//! in the name says whose it is, so a root whose process is gone is
//! garbage — and, for the e2e, a marker that its detached daemon and tmux
//! server may still be alive. A library module (not `#[cfg(test)]`) for
//! the same reason as `balerix_server::testing`: two crates' integration
//! tests share it.

use std::ops::Deref;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// How long `rm_rf` and `terminate` wait for a straggler.
const GRACE: Duration = Duration::from_secs(5);

/// The pid a test root or tmux socket is named after: the last
/// `-`-separated segment that parses as a number (`balerix-test-789-d` →
/// 789, `e2e-flow-456` → 456, `default` → none).
pub fn trailing_pid(name: &str) -> Option<u32> {
    name.rsplit('-').find_map(|segment| segment.parse().ok())
}

/// Whether the process exists and is not a zombie: `/proc/<pid>/stat`'s
/// state field, the character after the command's closing parenthesis. A
/// zombie owns nothing we could reap and only waits for its parent.
pub fn pid_alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(") ")
        .and_then(|(_, rest)| rest.chars().next())
        .is_some_and(|state| state != 'Z')
}

/// `remove_dir_all` with the materializer's bounded retry on
/// `DirectoryNotEmpty` (nono refills a tree it is still flushing).
pub fn rm_rf(path: &Path) -> std::io::Result<()> {
    crate::materializer::retry_rmdir(path, GRACE, Duration::from_millis(100), |p| {
        std::fs::remove_dir_all(p)
    })
}

/// Whether `name` is `<prefix>-…` on a segment boundary: `x` matches
/// `x-1` and `x-sub-1`, not `xylophone-1`.
fn has_prefix(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('-'))
}

/// Removes every `<dir>/<prefix>-…-<pid>` directory whose pid is dead and
/// returns them. A missing `dir` removes nothing.
pub fn sweep_dead_roots(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !has_prefix(&name, prefix) || !entry.path().is_dir() {
            continue;
        }
        let Some(pid) = trailing_pid(&name) else {
            continue;
        };
        if pid_alive(pid) {
            continue;
        }
        if rm_rf(&entry.path()).is_ok() {
            removed.push(entry.path());
        }
    }
    removed
}

/// A fresh `<dir>/<prefix>-<pid>` that removes itself when the test
/// passes and stays, with its logs, when the test panics. Creating one
/// first reaps the dead-pid roots of the same prefix that earlier runs
/// left behind.
pub struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    pub fn new(dir: &Path, prefix: &str) -> Self {
        sweep_dead_roots(dir, prefix);
        let path = dir.join(format!("{prefix}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)
            .unwrap_or_else(|e| panic!("{}: cannot create: {e}", path.display()));
        Self { path }
    }
}

impl Deref for TempRoot {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempRoot {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = rm_rf(&self.path);
        }
    }
}

/// Pids of every other process whose argv has `flag` immediately followed
/// by `value` — how a detached daemon is found by its `--tmux-socket
/// <name>` once its pid file is gone.
pub fn processes_with_arg_pair(flag: &str, value: &str) -> Vec<u32> {
    let me = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
        if args
            .windows(2)
            .any(|w| w[0] == flag.as_bytes() && w[1] == value.as_bytes())
        {
            found.push(pid);
        }
    }
    found.sort_unstable();
    found
}

/// SIGTERM every pid, wait up to `grace` for all of them to exit, then
/// SIGKILL the ones still there.
pub fn terminate(pids: &[u32], grace: Duration) {
    if pids.is_empty() {
        return;
    }
    let signal = |sig: &str| {
        for pid in pids {
            let _ = Command::new("kill").args([sig, &pid.to_string()]).status();
        }
    };
    signal("-TERM");
    let deadline = Instant::now() + grace;
    while pids.iter().any(|p| pid_alive(*p)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    if pids.iter().any(|p| pid_alive(*p)) {
        signal("-KILL");
    }
}

/// Where tmux keeps its sockets: `<$TMUX_TMPDIR or /tmp>/tmux-<uid>`.
pub fn tmux_socket_dir(tmux_tmpdir: Option<&Path>) -> PathBuf {
    let uid = std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(0);
    tmux_tmpdir
        .unwrap_or(Path::new("/tmp"))
        .join(format!("tmux-{uid}"))
}

/// Reaps what the test runs of `prefix` whose process is gone left
/// behind: for every socket `<socket_dir>/<prefix>-…-<pid>` with a dead
/// pid, the daemon still serving that socket (found by its argv), the
/// tmux server on it, and the socket file. Returns the socket names.
pub fn reap_dead_sessions(tmux: &Path, socket_dir: &Path, prefix: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(socket_dir) else {
        return Vec::new();
    };
    let mut reaped = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !has_prefix(&name, prefix) {
            continue;
        }
        let Some(pid) = trailing_pid(&name) else {
            continue;
        };
        if pid_alive(pid) {
            continue;
        }
        terminate(&processes_with_arg_pair("--tmux-socket", &name), GRACE);
        let _ = Command::new(tmux)
            .args(["-L", &name, "kill-server"])
            .output();
        let _ = std::fs::remove_file(entry.path());
        reaped.push(name);
    }
    reaped.sort();
    reaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn dead_pid() -> u32 {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    /// Polls `pred` every 10 ms for up to 5 s: a spawned `sh` and the
    /// `sleep` it forks each carry the parent's argv until they exec, so
    /// a `/proc` scan taken at once may see one pid too many or too few.
    fn eventually(mut pred: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if pred() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn trailing_pid_reads_the_last_numeric_segment() {
        assert_eq!(trailing_pid("e2e-123"), Some(123));
        assert_eq!(trailing_pid("e2e-flow-456"), Some(456));
        assert_eq!(trailing_pid("balerix-test-789-d"), Some(789));
        assert_eq!(trailing_pid("materialize-noauth-1"), Some(1));
        assert_eq!(trailing_pid("serve"), None);
        assert_eq!(trailing_pid("e2e-abc"), None);
        assert_eq!(trailing_pid("default"), None);
    }

    #[test]
    fn pid_alive_knows_this_process_a_reaped_child_and_a_zombie() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(dead_pid()));
        let mut zombie = Command::new("true").spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while pid_alive(zombie.id()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !pid_alive(zombie.id()),
            "an exited, unreaped child is a zombie"
        );
        zombie.wait().unwrap();
    }

    #[test]
    fn sweep_removes_dead_roots_of_the_prefix_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let dead = dead_pid();
        for name in [
            format!("x-{me}"),
            format!("x-{dead}"),
            format!("x-sub-{dead}"),
            "x-notapid".to_string(),
            format!("y-{dead}"),
            format!("xylophone-{dead}"),
        ] {
            std::fs::create_dir_all(dir.path().join(name).join("inner")).unwrap();
        }
        let removed = sweep_dead_roots(dir.path(), "x");
        let mut names: Vec<String> = removed
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec![format!("x-{dead}"), format!("x-sub-{dead}")]);
        assert!(dir.path().join(format!("x-{me}")).exists(), "live pid kept");
        assert!(dir.path().join("x-notapid").exists(), "no pid: kept");
        assert!(
            dir.path().join(format!("y-{dead}")).exists(),
            "other prefix kept"
        );
        assert!(
            dir.path().join(format!("xylophone-{dead}")).exists(),
            "prefix matches whole segments only"
        );
        assert!(sweep_dead_roots(dir.path().join("missing").as_path(), "x").is_empty());
    }

    #[test]
    fn temp_root_is_named_by_prefix_and_pid_and_removed_when_the_test_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = {
            let root = TempRoot::new(dir.path(), "t");
            assert_eq!(
                root.file_name().unwrap().to_string_lossy(),
                format!("t-{}", std::process::id())
            );
            std::fs::write(root.join("f"), b"x").unwrap();
            assert!(root.is_dir());
            root.to_path_buf()
        };
        assert!(!path.exists(), "dropped without a panic: removed");
    }

    #[test]
    fn temp_root_stays_when_the_test_panics() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let joined = std::thread::spawn(move || {
            let root = TempRoot::new(&base, "p");
            std::fs::write(root.join("evidence"), b"x").unwrap();
            panic!("boom");
        })
        .join();
        assert!(joined.is_err());
        let kept = dir.path().join(format!("p-{}", std::process::id()));
        assert!(
            kept.join("evidence").exists(),
            "a failed test keeps its root"
        );
    }

    #[test]
    fn processes_with_arg_pair_finds_a_live_child_by_its_argv() {
        let marker = format!("marker-{}", std::process::id());
        let mut child = Command::new("sh")
            .args(["-c", "sleep 5; true", "sh", "--tmux-socket", &marker])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let expected = vec![child.id()];
        let settled = eventually(|| processes_with_arg_pair("--tmux-socket", &marker) == expected);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            settled,
            "the child (and only the child) carries the marker once it has exec'd"
        );
        assert!(processes_with_arg_pair("--tmux-socket", "nobody-has-this").is_empty());
    }

    #[test]
    fn reap_kills_the_daemon_of_a_dead_socket_and_removes_the_file() {
        let sockets = tempfile::tempdir().unwrap();
        let dead = dead_pid();
        let name = format!("balerix-e2e-{dead}");
        std::fs::write(sockets.path().join(&name), b"").unwrap();
        std::fs::write(sockets.path().join("balerix-e2e-notapid"), b"").unwrap();
        let live = format!("balerix-e2e-{}", std::process::id());
        std::fs::write(sockets.path().join(&live), b"").unwrap();
        let mut daemon = Command::new("sh")
            .args(["-c", "sleep 5; true", "sh", "--tmux-socket", &name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        assert!(
            eventually(|| processes_with_arg_pair("--tmux-socket", &name) == vec![daemon.id()]),
            "the daemon is visible once it has exec'd"
        );

        let reaped = reap_dead_sessions(Path::new("tmux"), sockets.path(), "balerix-e2e");
        assert_eq!(reaped, vec![name.clone()]);
        assert!(!sockets.path().join(&name).exists(), "socket file removed");
        assert!(sockets.path().join("balerix-e2e-notapid").exists());
        assert!(
            sockets.path().join(&live).exists(),
            "a live run's socket is kept"
        );
        let status = daemon.wait().unwrap();
        assert!(!status.success(), "the daemon was signalled: {status}");
        assert!(reap_dead_sessions(Path::new("tmux"), sockets.path(), "balerix-e2e").is_empty());
    }

    #[test]
    fn tmux_socket_dir_is_under_tmux_tmpdir_or_tmp_and_named_by_uid() {
        let d = tmux_socket_dir(Some(Path::new("/x")));
        assert!(d.starts_with("/x"), "{d:?}");
        assert!(
            d.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("tmux-")
        );
        assert!(tmux_socket_dir(None).starts_with("/tmp"));
    }
}
