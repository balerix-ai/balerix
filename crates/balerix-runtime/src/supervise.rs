//! The agent supervisor (Spec N amendment §13): `balerix agent-supervise`
//! runs an agent's command as a child subreaper, so every process the
//! agent detaches re-parents here and not to pid 1, and ends the whole
//! tree on stop or when the main child exits. Also the process identity
//! `TmuxRunner` polls to learn that the wrapper is gone.

use std::ffi::OsString;
use std::io;
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
        let me = ProcIdentity::of(std::process::id()).unwrap();
        assert_eq!(me.pid, std::process::id());
        assert!(!me.gone());
    }

    #[test]
    fn a_zombie_and_a_reaped_pid_are_gone() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let id = ProcIdentity::of(child.id()).unwrap();
        assert!(!id.gone());
        child.kill().unwrap();
        // not waited yet: a zombie, which has exited as far as a stop cares
        let start = std::time::Instant::now();
        while !id.gone() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "never a zombie"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            ProcIdentity::of(child.id()).is_none(),
            "a zombie has no identity"
        );
        child.wait().unwrap();
        assert!(id.gone(), "and stays gone once reaped");
    }

    #[test]
    fn a_reused_pid_is_a_different_process() {
        let me = ProcIdentity::of(std::process::id()).unwrap();
        let other = ProcIdentity {
            start: me.start + 1,
            ..me
        };
        assert!(other.gone(), "same pid, another start time");
    }

    #[test]
    fn exit_codes_follow_the_shell_convention() {
        assert_eq!(exit_code(Some(7), None), 7);
        assert_eq!(exit_code(None, Some(9)), 137);
        assert_eq!(exit_code(None, None), 1);
    }
}
