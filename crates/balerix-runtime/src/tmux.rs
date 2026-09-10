//! `AgentRunner` over a dedicated tmux server (Phase 2 spec §4.3).
//!
//! `ensure_agent` diverges from the spec's §4.3 table, which shows a window
//! created (or respawned) directly with the agent's script: instead it
//! always brings the window to an idle, live pane first (creating it idle if
//! absent, or `respawn-window`-ing it into idle if its pane already exited)
//! and only then attaches `pipe-pane` and `respawn-window`s into the real
//! script. tmux reads (and, once read, discards) a pane's pty output as soon
//! as the child produces it, so a script that writes immediately can race
//! ahead of a `pipe-pane` attached to that same window after it was started
//! with the real command, losing that output to the log forever; going
//! through an idle pane first, and only ever attaching `pipe-pane` to a live
//! one, removes the race (a dead pane refuses `pipe-pane` outright).

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use balerix_core::{
    AgentId, AgentName, AgentRunner, CrewName, CrewRef, FleetName, LaunchPlan, ObservedState,
    ProcessState, PtyStream, RunnerError,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::tools::Cmd;

/// The window that keeps a session alive when every agent window is gone.
/// `Fleet::try_from` reserves this name so no agent window can collide.
pub const ANCHOR_WINDOW: &str = balerix_core::RESERVED_AGENT_NAME;
const WINDOW_FORMAT: &str = "#{window_name}\t#{pane_dead}\t#{pane_pid}\t#{pane_dead_status}";
/// Command for a window that should sit idle: the crew anchor, and an
/// agent's window between creation and its first `respawn-window` into the
/// real script.
const IDLE_ARGV: [&str; 3] = ["/bin/sh", "-c", "while :; do sleep 3600; done"];
/// Prefix of the throwaway grouped session one attach creates (plugins
/// spec §18.4); `observe` ignores it, `stop_crew` kills it with the crew.
pub const ATTACH_SESSION_PREFIX: &str = "balerix-attach-";
const ATTACH_TERM: &str = "xterm-256color";
static ATTACH_SEQ: AtomicU64 = AtomicU64::new(0);
static SEND_SEQ: AtomicU64 = AtomicU64::new(0);
/// Above this many bytes a single line is pasted through a buffer like a
/// multi-line text: tmux refuses a command whose argv exceeds ~16 KiB
/// (`command too long`), and `send-keys -l` carries the text as argv.
pub(crate) const SEND_KEYS_LIMIT: usize = 4096;

/// `balerix-attach-<8 hex>`, unique per process: the clock, a counter and
/// the pid folded into 32 bits.
fn attach_session_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    attach_name(
        nanos,
        ATTACH_SEQ.fetch_add(1, Ordering::Relaxed),
        std::process::id(),
    )
}

/// The name's arithmetic, without the clock or the process: the three
/// inputs are xored apart (the counter into the high half, so two names
/// taken within one clock tick still differ) and then the high half is
/// folded back down, or everything the counter contributes would be
/// truncated away.
fn attach_name(nanos: u64, seq: u64, pid: u32) -> String {
    let mixed = nanos ^ (seq << 32) ^ u64::from(pid);
    let low = u32::try_from((mixed ^ (mixed >> 32)) & 0xffff_ffff).unwrap_or(0);
    format!("{ATTACH_SESSION_PREFIX}{low:08x}")
}

/// The session names of `crew`'s group: itself and every session grouped
/// with it. An ungrouped session's `session_group` is empty.
pub(crate) fn sessions_in_group(text: &str, crew: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let (name, group) = l.split_once('\t')?;
            (name == crew || group == crew).then(|| name.to_string())
        })
        .collect()
}

/// One attached client: a tmux client in a PTY on a throwaway session
/// grouped with the crew's, so viewers never fight the operator's own
/// client over the current window. Drop kills the client and the
/// session; if the daemon dies first, the PTY closes, the client detaches
/// and tmux's `destroy-unattached` finishes the job.
pub struct TmuxAttach {
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// The port says the writer is taken once; enforced here rather than
    /// relying on what `portable-pty` does on a second `take_writer`.
    writer_taken: AtomicBool,
    tmux: PathBuf,
    socket: String,
    session: String,
}

impl PtyStream for TmuxAttach {
    fn reader(&self) -> io::Result<Box<dyn Read + Send>> {
        self.master.try_clone_reader().map_err(io::Error::other)
    }
    fn writer(&self) -> io::Result<Box<dyn Write + Send>> {
        if self.writer_taken.swap(true, Ordering::SeqCst) {
            return Err(io::Error::other("the writer was already taken"));
        }
        self.master.take_writer().map_err(io::Error::other)
    }
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)
    }
}

impl Drop for TmuxAttach {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = Cmd::new(&self.tmux)
            .args([
                "-L".to_string(),
                self.socket.clone(),
                "kill-session".to_string(),
                "-t".to_string(),
                format!("={}", self.session),
            ])
            .run();
    }
}

pub struct TmuxRunner {
    pub tmux: PathBuf,
    pub socket: String,
}

impl TmuxRunner {
    pub fn new(tmux: PathBuf, socket: impl Into<String>) -> Self {
        Self {
            tmux,
            socket: socket.into(),
        }
    }

    fn cmd(&self) -> Cmd {
        Cmd::new(&self.tmux).args(["-L".to_string(), self.socket.clone()])
    }

    fn run(&self, id: &str, args: &[&str]) -> Result<String, RunnerError> {
        self.cmd()
            .args(args.iter().copied())
            .run()
            .map(|o| o.stdout)
            .map_err(|f| RunnerError::Tool {
                id: id.to_string(),
                subcommand: f.subcommand,
                args: f.args,
                stderr: f.stderr,
            })
    }

    /// `run`, with `input` handed to the tmux client on its stdin: the
    /// client packs a command's argv into a single message and refuses one
    /// over 16 KiB ("command too long"), so a review-sized text cannot be
    /// an argument of `set-buffer` and arrives through `load-buffer -`
    /// instead.
    fn run_with_stdin(&self, id: &str, args: &[&str], input: &[u8]) -> Result<(), RunnerError> {
        self.cmd()
            .args(args.iter().copied())
            .run_with_stdin(input)
            .map(|_| ())
            .map_err(|f| RunnerError::Tool {
                id: id.to_string(),
                subcommand: f.subcommand,
                args: f.args,
                stderr: f.stderr,
            })
    }

    /// tmux exits non-zero with "no server running" / "can't find" /
    /// "error connecting" when nothing exists, and with "no current
    /// target" when the server is up but holds no session at all (the
    /// moment between another crew's `new-session` starting the server and
    /// its session existing — two crews on one socket race there at daemon
    /// start); all of those are "absent", not errors.
    fn run_optional(&self, id: &str, args: &[&str]) -> Result<Option<String>, RunnerError> {
        match self.cmd().args(args.iter().copied()).run() {
            Ok(o) => Ok(Some(o.stdout)),
            Err(f)
                if f.stderr.contains("no server running")
                    || f.stderr.contains("can't find")
                    || f.stderr.contains("error connecting")
                    || f.stderr.contains("no current target") =>
            {
                Ok(None)
            }
            Err(f) => Err(RunnerError::Tool {
                id: id.to_string(),
                subcommand: f.subcommand,
                args: f.args,
                stderr: f.stderr,
            }),
        }
    }

    fn session_target(crew: &CrewRef) -> String {
        format!("={crew}")
    }

    fn window_target(agent: &AgentId) -> String {
        format!("={}:={}", agent.crew_ref(), agent.agent)
    }

    fn windows(
        &self,
        crew: &CrewRef,
    ) -> Result<Option<BTreeMap<AgentName, ProcessState>>, RunnerError> {
        let Some(text) = self.run_optional(
            &crew.to_string(),
            &[
                "list-windows",
                "-t",
                &Self::session_target(crew),
                "-F",
                WINDOW_FORMAT,
            ],
        )?
        else {
            return Ok(None);
        };
        parse_windows(&crew.fleet, &crew.crew, &text).map(Some)
    }
}

pub(crate) fn parse_windows(
    fleet: &FleetName,
    crew: &CrewName,
    text: &str,
) -> Result<BTreeMap<AgentName, ProcessState>, RunnerError> {
    let id = format!("{fleet}/{crew}");
    let mut out = BTreeMap::new();
    for line in text.lines().filter(|l| !l.is_empty()) {
        let cols: Vec<&str> = line.split('\t').collect();
        let [name, dead, pid, status] = cols[..] else {
            return Err(RunnerError::Parse {
                id,
                message: format!("expected 4 columns in {line:?}"),
            });
        };
        if name == ANCHOR_WINDOW {
            continue;
        }
        let Ok(agent) = name.parse::<AgentName>() else {
            continue; // a window we did not create; ignore
        };
        let state = if dead == "1" {
            ProcessState::Exited {
                code: status.parse().ok(),
            }
        } else {
            ProcessState::Running {
                pid: pid.parse().map_err(|_| RunnerError::Parse {
                    id: id.clone(),
                    message: format!("bad pid in {line:?}"),
                })?,
            }
        };
        out.insert(agent, state);
    }
    Ok(out)
}

impl AgentRunner for TmuxRunner {
    fn ensure_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        if self
            .run_optional(
                &crew.to_string(),
                &["has-session", "-t", &Self::session_target(crew)],
            )?
            .is_some()
        {
            return Ok(());
        }
        self.run(
            &crew.to_string(),
            &[
                "new-session",
                "-d",
                "-s",
                &crew.to_string(),
                "-n",
                ANCHOR_WINDOW,
                "--",
                IDLE_ARGV[0],
                IDLE_ARGV[1],
                IDLE_ARGV[2],
            ],
        )
        .map(|_| ())
    }

    fn ensure_agent(&self, agent: &AgentId, plan: &LaunchPlan) -> Result<(), RunnerError> {
        let id = agent.to_string();
        let crew = agent.crew_ref();
        let state = self
            .windows(&crew)?
            .and_then(|w| w.get(&agent.agent).copied());
        let cwd = plan.cwd.display().to_string();
        let script = plan.script.display().to_string();
        let target = Self::window_target(agent);
        match state {
            None => {
                // Window absent: create it idle, not with the real script
                // directly. See the module doc for why (a `pipe-pane`
                // output-loss race).
                self.run(
                    &id,
                    &[
                        "new-window",
                        "-d",
                        "-t",
                        &Self::session_target(&crew),
                        "-n",
                        agent.agent.as_str(),
                        "-c",
                        &cwd,
                        "--",
                        IDLE_ARGV[0],
                        IDLE_ARGV[1],
                        IDLE_ARGV[2],
                    ],
                )?;
            }
            Some(ProcessState::Exited { .. }) => {
                // Window exists but its pane already exited: tmux's
                // `pipe-pane` refuses a dead pane ("target pane has
                // exited"), so revive it into the idle placeholder first,
                // for the same reason as the `None` arm above.
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
            }
            Some(ProcessState::Running { .. }) => {}
        }
        // `set-option` is idempotent and, by this point, always aimed at a
        // live pane, so re-applying it here unconditionally (not only right
        // after creating or reviving the window above) repairs a window
        // that exists without it: one left by a create that failed between
        // here and `respawn-window` below, one from before this repair
        // existed, or one an operator created by hand. Without this, such a
        // window would never gain remain-on-exit, and the reconciler would
        // recreate/restart it forever.
        self.run(
            &id,
            &["set-option", "-w", "-t", &target, "remain-on-exit", "on"],
        )?;
        let log = plan
            .script
            .parent()
            .map(|p| p.join("logs").join("tmux.log"))
            .unwrap_or_else(|| PathBuf::from("tmux.log"));
        // No `-o` here: tmux tracks "already piping" on the pane itself, and
        // that flag survives both the pane dying and `respawn-window`
        // reviving it with a fresh pty. `-o` reads that stale flag, believes
        // a pipe is already attached, and silently drops the (already-dead)
        // one without opening a new one — the revived pane then never gets
        // piped at all. Without `-o`, `pipe-pane` always closes any existing
        // pipe and opens this one, which is exactly what every call site
        // wants: this runs immediately before `respawn-window` starts the
        // process whose output we actually want captured, so there is never
        // a live process (from *this* pane, going forward) whose output
        // could fall in the close/reopen gap.
        self.run(
            &id,
            &[
                "pipe-pane",
                "-t",
                &target,
                &format!(
                    "cat >> {}",
                    crate::quote::sh_quote(&log.display().to_string())
                ),
            ],
        )?;
        self.run(
            &id,
            &["respawn-window", "-k", "-t", &target, "-c", &cwd, &script],
        )
        .map(|_| ())
    }

    fn stop_agent(&self, agent: &AgentId) -> Result<(), RunnerError> {
        self.run_optional(
            &agent.to_string(),
            &["kill-window", "-t", &Self::window_target(agent)],
        )
        .map(|_| ())
    }

    fn stop_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        let name = crew.to_string();
        // `kill-session` on the crew session alone would leave its
        // windows alive in any grouped attach session (§18.4): every
        // session of the group goes. The list and the kills are separate
        // commands, so an attach created in between survives with the
        // windows linked into it while `observe` reports the crew gone;
        // the reconciler's next pass finds and stops it. tmux has no
        // "kill the group" command to make this one step.
        let Some(text) = self.run_optional(
            &name,
            &["list-sessions", "-F", "#{session_name}\t#{session_group}"],
        )?
        else {
            return Ok(());
        };
        for session in sessions_in_group(&text, &name) {
            self.run_optional(&name, &["kill-session", "-t", &format!("={session}")])?;
        }
        Ok(())
    }

    fn observe(&self, fleet: &FleetName) -> Result<ObservedState, RunnerError> {
        let mut out = ObservedState::default();
        let Some(sessions) =
            self.run_optional(fleet.as_str(), &["list-sessions", "-F", "#{session_name}"])?
        else {
            return Ok(out);
        };
        let prefix = format!("{fleet}/");
        for name in sessions.lines() {
            let Some(crew_name) = name.strip_prefix(&prefix) else {
                continue;
            };
            let Ok(crew) = crew_name.parse::<CrewName>() else {
                continue;
            };
            let crew_ref = CrewRef {
                fleet: fleet.clone(),
                crew: crew.clone(),
            };
            let windows = self.windows(&crew_ref)?.unwrap_or_default();
            out.crews.insert(crew, windows);
        }
        Ok(out)
    }

    /// One short line goes through `send-keys -l`; text with a newline, or
    /// longer than `SEND_KEYS_LIMIT`, goes through a named buffer and
    /// `paste-buffer -p` (Spec C §3.3): `-p`
    /// wraps it in bracketed-paste markers when the application asked for
    /// them, on which a multi-line paste is expected to arrive as one
    /// message (verify with `mise run verify-claude`; Spec C §8, pending);
    /// `-d` deletes the buffer. The buffer is filled by `load-buffer -`
    /// from the tmux client's stdin, not by `set-buffer -- <text>`: the
    /// client refuses a command whose packed argv exceeds 16 KiB, and
    /// Spec C §4.4 allows a review of 64 KiB. Buffer names are unique per
    /// process so two concurrent sends cannot swap texts; on a failed
    /// paste (the target window gone, say) the buffer is deleted before
    /// the error is returned, so failures do not leak buffers.
    fn send_text(&self, agent: &AgentId, text: &str, submit: bool) -> Result<(), RunnerError> {
        let id = agent.to_string();
        let target = Self::window_target(agent);
        if text.contains('\n') || text.len() > SEND_KEYS_LIMIT {
            let buffer = format!(
                "balerix-send-{}-{}",
                std::process::id(),
                SEND_SEQ.fetch_add(1, Ordering::Relaxed)
            );
            self.run_with_stdin(&id, &["load-buffer", "-b", &buffer, "-"], text.as_bytes())?;
            if let Err(err) = self.run(
                &id,
                &["paste-buffer", "-p", "-d", "-b", &buffer, "-t", &target],
            ) {
                let _ = self.run(&id, &["delete-buffer", "-b", &buffer]);
                return Err(err);
            }
        } else {
            self.run(&id, &["send-keys", "-t", &target, "-l", "--", text])?;
        }
        if submit {
            self.run(&id, &["send-keys", "-t", &target, "Enter"])?;
        }
        Ok(())
    }

    fn attach(&self, agent: &AgentId) -> Result<Box<dyn PtyStream>, RunnerError> {
        let id = agent.to_string();
        let crew = agent.crew_ref();
        let session = attach_session_name();
        let fail = |stderr: String| RunnerError::Tool {
            id: id.clone(),
            subcommand: "attach-session".into(),
            args: vec![session.clone()],
            stderr,
        };
        // The window must exist: `select-window` on a missing one would
        // leave the client on the anchor.
        let known = self
            .windows(&crew)?
            .is_some_and(|w| w.contains_key(&agent.agent));
        if !known {
            return Err(fail(format!("no window for {agent}")));
        }
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| fail(format!("openpty: {e}")))?;
        // One command sequence, with the client attached before
        // `destroy-unattached` is set: tmux destroys a detached session the
        // moment that option lands on it.
        let mut cmd = CommandBuilder::new(&self.tmux);
        cmd.args([
            "-L".to_string(),
            self.socket.clone(),
            "new-session".to_string(),
            "-t".to_string(),
            Self::session_target(&crew),
            "-s".to_string(),
            session.clone(),
            ";".to_string(),
            "select-window".to_string(),
            "-t".to_string(),
            format!("={}", agent.agent),
            ";".to_string(),
            "set-option".to_string(),
            "destroy-unattached".to_string(),
            "on".to_string(),
            ";".to_string(),
            "set-option".to_string(),
            "status".to_string(),
            "off".to_string(),
        ]);
        cmd.env("TERM", ATTACH_TERM);
        cmd.cwd("/");
        let child = pty
            .slave
            .spawn_command(cmd)
            .map_err(|e| fail(format!("spawn: {e}")))?;
        drop(pty.slave);
        let mut attach = TmuxAttach {
            master: pty.master,
            child,
            writer_taken: AtomicBool::new(false),
            tmux: self.tmux.clone(),
            socket: self.socket.clone(),
            session: session.clone(),
        };
        // Everything after the `windows()` check was fire-and-forget: a
        // duplicate name or a crew session killed meanwhile would hand
        // out a live stream whose reader shows tmux's error line and then
        // EOF. The session appearing is what says the client attached (a
        // `select-window` failing after that leaves it on the anchor, not
        // dead); on failure `attach` drops here and cleans up after
        // itself.
        self.confirm_attached(&mut attach).map_err(fail)?;
        Ok(Box::new(attach))
    }
}

/// How long an attach client gets to bring its session up.
const ATTACH_CONFIRM: Duration = Duration::from_secs(5);

impl TmuxRunner {
    fn confirm_attached(&self, attach: &mut TmuxAttach) -> Result<(), String> {
        let target = format!("={}", attach.session);
        let start = Instant::now();
        loop {
            if self
                .cmd()
                .args(["has-session", "-t", target.as_str()])
                .run()
                .is_ok()
            {
                return Ok(());
            }
            if let Ok(Some(status)) = attach.child.try_wait() {
                return Err(format!(
                    "the tmux client exited ({status:?}) before its session appeared"
                ));
            }
            if start.elapsed() > ATTACH_CONFIRM {
                return Err(format!(
                    "the attach session did not appear within {ATTACH_CONFIRM:?}"
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_running_dead_and_skips_anchor_and_foreign_windows() {
        let f: FleetName = "f".parse().unwrap();
        let c: CrewName = "c".parse().unwrap();
        let text = "balerix\t0\t10\t\nalice\t0\t42\t\nbob\t1\t43\t137\nNot_An_Agent\t0\t1\t\n";
        let w = parse_windows(&f, &c, text).unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!(
            w[&"alice".parse::<AgentName>().unwrap()],
            ProcessState::Running { pid: 42 }
        );
        assert_eq!(
            w[&"bob".parse::<AgentName>().unwrap()],
            ProcessState::Exited { code: Some(137) }
        );
        assert!(
            parse_windows(&f, &c, "garbage")
                .unwrap_err()
                .to_string()
                .contains("expected 4 columns")
        );
    }

    #[test]
    fn a_crew_group_is_the_crew_session_and_everything_grouped_with_it() {
        let text =
            "f/c\tf/c\nbalerix-attach-1a2b3c4d\tf/c\nf/d\t\ng/c\tg/c\nbalerix-attach-9\tg/c\n";
        assert_eq!(
            sessions_in_group(text, "f/c"),
            vec!["f/c", "balerix-attach-1a2b3c4d"]
        );
        assert_eq!(
            sessions_in_group(text, "f/d"),
            vec!["f/d"],
            "ungrouped: itself"
        );
        assert!(sessions_in_group(text, "f/e").is_empty());
        assert!(attach_session_name().starts_with(ATTACH_SESSION_PREFIX));
        assert_ne!(attach_session_name(), attach_session_name());
        // The counter alone must change the name: two attaches within one
        // clock tick, in one process, would otherwise collide.
        assert_ne!(attach_name(1, 0, 7), attach_name(1, 1, 7));
        assert_ne!(attach_name(1, 0, 7), attach_name(2, 0, 7), "and the clock");
        assert_ne!(attach_name(1, 0, 7), attach_name(1, 0, 8), "and the pid");
    }

    #[test]
    fn targets_use_exact_match_prefixes() {
        let a: AgentId = "f/c/a".parse().unwrap();
        assert_eq!(TmuxRunner::session_target(&a.crew_ref()), "=f/c");
        assert_eq!(TmuxRunner::window_target(&a), "=f/c:=a");
    }
}
