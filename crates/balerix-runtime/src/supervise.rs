//! The agent supervisor (Spec N amendment §13): `balerix agent-supervise`
//! runs an agent's command as a child subreaper, so every process the
//! agent detaches re-parents here and not to pid 1, and ends the whole
//! tree on stop or when the main child exits. Also the process identity
//! `TmuxRunner` polls to learn that the wrapper is gone.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitOptions, getpid, kill_process, set_child_subreaper, wait};

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
/// The longest pause, once passes keep finding the same pids (#120).
const KILL_STEP_MAX: Duration = Duration::from_millis(100);
/// How many passes in a row may find the same pids before the pause grows.
const SAME_PASSES: usize = 5;

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

/// One process, told apart from a later one that reuses its pid by its
/// start time. A zombie has no identity: it has exited, and only waits
/// for its parent to reap it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcIdentity {
    pub pid: u32,
    start: u64,
}

/// Where process stats are read: `/proc`, or a fake one in a test.
#[derive(Debug, Clone)]
pub struct Procfs {
    root: PathBuf,
}

impl Default for Procfs {
    fn default() -> Self {
        Self::at("/proc")
    }
}

impl Procfs {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `None` once the process has exited and its entry is gone. Any other
    /// failure to read or parse the stat is an error, never an exit
    /// (#148): under `hidepid`, or for another uid's process, a live
    /// process would otherwise read as gone.
    fn stat(&self, pid: u32) -> io::Result<Option<Stat>> {
        let path = self.root.join(pid.to_string()).join("stat");
        match std::fs::read_to_string(&path) {
            Ok(text) => parse_stat(&text).map(Some).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: cannot parse {text:?}", path.display()),
                )
            }),
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    || e.raw_os_error() == Some(Errno::SRCH.raw_os_error()) =>
            {
                Ok(None)
            }
            Err(e) => Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
        }
    }

    /// `(pid, ppid)` of every process this shows. A process that exits
    /// while this reads is skipped, and so is one whose stat cannot be
    /// read (another uid's, under `hidepid=1`): a descendant of the
    /// wrapper runs as its uid. Not being able to list at all is an error
    /// (#120): the kill loop would see nothing and never end.
    pub(crate) fn parents(&self) -> io::Result<Vec<(u32, u32)>> {
        let entries = std::fs::read_dir(&self.root)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", self.root.display())))?;
        Ok(entries
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .filter_map(|pid| Some((pid, self.stat(pid).ok()??.ppid)))
            .collect())
    }

    /// `None` when `pid` has exited, a zombie included.
    pub fn identity(&self, pid: u32) -> io::Result<Option<ProcIdentity>> {
        Ok(self
            .stat(pid)?
            .filter(|s| s.state != 'Z')
            .map(|s| ProcIdentity {
                pid,
                start: s.start,
            }))
    }

    /// The process has exited, whether or not its pid is in use again.
    pub fn gone(&self, id: &ProcIdentity) -> io::Result<bool> {
        Ok(self.identity(id.pid)? != Some(*id))
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

/// The kill loop's pause (#120). `KILL_STEP` while each pass finds a
/// different set of pids, so a tree that forks faster than one scan is
/// still emptied; once `SAME_PASSES` passes in a row found the same set
/// (a descendant that cannot die yet, in uninterruptible sleep on NFS or
/// FUSE), the pause doubles up to `KILL_STEP_MAX`. Anything new resets it.
#[derive(Debug, Default)]
struct KillBackoff {
    last: Vec<u32>,
    same: usize,
    pause: Duration,
}

impl KillBackoff {
    fn after(&mut self, found: &[u32]) -> Duration {
        let mut found = found.to_vec();
        found.sort_unstable();
        if found == self.last {
            self.same += 1;
        } else {
            self.last = found;
            self.same = 0;
        }
        self.pause = if self.same < SAME_PASSES {
            KILL_STEP
        } else {
            (self.pause * 2).min(KILL_STEP_MAX)
        };
        self.pause
    }
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
        // `wait`, not `waitpid(None, ..)`: that only sees children in this
        // process group, and a `setsid` orphan is in another.
        match wait(WaitOptions::NOHANG) {
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
/// It does not give up: the caller of `stop` has the bound (§13.5). A
/// descendant that will not die costs a bounded scan rate (`KillBackoff`).
fn kill_until_empty(main: u32, status: &mut Option<i32>) -> io::Result<()> {
    let me = raw(getpid());
    let proc = Procfs::default();
    let mut backoff = KillBackoff::default();
    while reap(main, status)? {
        let found = descendants_of(me, &proc.parents()?);
        for &pid in &found {
            if let Some(pid) = pid_of(pid) {
                // gone already is fine; the next pass sees what is left
                let _ = kill_process(pid, Signal::KILL);
            }
        }
        std::thread::sleep(backoff.after(&found));
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_stat_survives_a_command_name_with_parentheses_and_spaces() {
        // field 2 is the command name in parentheses and may itself hold
        // `) `; the fields after the LAST `) ` are state, ppid, …, and
        // starttime is field 22
        let text =
            "4242 (a) b (c) S 17 4242 4242 0 -1 4194304 1 2 3 4 5 6 7 8 20 0 1 0 987654 10 11";
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
        let proc = Procfs::default();
        let me = proc.identity(std::process::id()).unwrap().unwrap();
        assert_eq!(me.pid, std::process::id());
        assert!(!proc.gone(&me).unwrap());
    }

    #[test]
    fn a_zombie_and_a_reaped_pid_are_gone() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let proc = Procfs::default();
        let id = proc.identity(child.id()).unwrap().unwrap();
        assert!(!proc.gone(&id).unwrap());
        child.kill().unwrap();
        // not waited yet: a zombie, which has exited as far as a stop cares
        let start = std::time::Instant::now();
        while !proc.gone(&id).unwrap() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "never a zombie"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            proc.identity(child.id()).unwrap().is_none(),
            "a zombie has no identity"
        );
        child.wait().unwrap();
        assert!(proc.gone(&id).unwrap(), "and stays gone once reaped");
    }

    #[test]
    fn a_reused_pid_is_a_different_process() {
        let proc = Procfs::default();
        let me = proc.identity(std::process::id()).unwrap().unwrap();
        let other = ProcIdentity {
            start: me.start + 1,
            ..me
        };
        assert!(proc.gone(&other).unwrap(), "same pid, another start time");
    }

    /// A fake `/proc` holding one stat line for `pid`.
    fn fake_stat(root: &std::path::Path, pid: u32, line: &str) {
        let dir = root.join(pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stat"), line).unwrap();
    }

    fn stat_line(pid: u32, state: char, start: u64) -> String {
        format!("{pid} (x) {state} 1 {pid} {pid} 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {start} 1 1")
    }

    #[test]
    fn a_missing_stat_is_an_exited_process_and_a_garbled_one_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let proc = Procfs::at(dir.path());
        assert!(
            proc.identity(7).unwrap().is_none(),
            "no /proc entry: exited"
        );
        fake_stat(dir.path(), 8, "8 (x) S");
        let err = proc.identity(8).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        fake_stat(dir.path(), 9, &stat_line(9, 'Z', 5));
        assert!(proc.identity(9).unwrap().is_none(), "a zombie has exited");
    }

    #[test]
    fn an_unreadable_stat_is_an_error_not_an_exit() {
        if rustix::process::geteuid().is_root() {
            // root reads a mode-000 file; nothing to test
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        fake_stat(dir.path(), 7, &stat_line(7, 'S', 5));
        let stat = dir.path().join("7").join("stat");
        std::fs::set_permissions(&stat, std::fs::Permissions::from_mode(0o000)).unwrap();
        let err = Procfs::at(dir.path()).identity(7).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
    }

    #[test]
    fn gone_is_an_error_when_the_stat_turns_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let proc = Procfs::at(dir.path());
        fake_stat(dir.path(), 7, &stat_line(7, 'S', 5));
        let id = proc.identity(7).unwrap().unwrap();
        assert!(!proc.gone(&id).unwrap());
        fake_stat(dir.path(), 7, "garbage");
        assert!(proc.gone(&id).is_err(), "unreadable is not gone");
        std::fs::remove_dir_all(dir.path().join("7")).unwrap();
        assert!(proc.gone(&id).unwrap(), "no entry: gone");
    }

    #[test]
    fn an_unreadable_proc_is_an_error_and_an_unreadable_pid_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let missing = Procfs::at(dir.path().join("nope"));
        assert!(
            missing.parents().is_err(),
            "no /proc: the kill loop cannot see"
        );

        let proc = Procfs::at(dir.path());
        fake_stat(dir.path(), 7, &stat_line(7, 'S', 5));
        fake_stat(dir.path(), 8, "garbage");
        std::fs::create_dir_all(dir.path().join("self")).unwrap();
        assert_eq!(proc.parents().unwrap(), vec![(7, 1)]);
    }

    #[test]
    fn the_kill_loop_slows_only_while_it_finds_the_same_pids() {
        let mut b = KillBackoff::default();
        // a changing set (a fork race) keeps the full rate
        for n in 0..20 {
            assert_eq!(b.after(&[n]), KILL_STEP, "pass {n}");
        }
        // the same set: full rate for a few passes, then doubling to a cap
        let pauses: Vec<_> = (0..12).map(|_| b.after(&[19])).collect();
        assert!(
            pauses[..SAME_PASSES - 1].iter().all(|p| *p == KILL_STEP),
            "{pauses:?}"
        );
        assert!(pauses.windows(2).all(|w| w[1] >= w[0]), "{pauses:?}");
        assert_eq!(*pauses.last().unwrap(), KILL_STEP_MAX);
        // anything new: back to full rate at once
        assert_eq!(b.after(&[19, 20]), KILL_STEP);
    }

    #[test]
    fn exit_codes_follow_the_shell_convention() {
        assert_eq!(exit_code(Some(7), None), 7);
        assert_eq!(exit_code(None, Some(9)), 137);
        assert_eq!(exit_code(None, None), 1);
    }
}
