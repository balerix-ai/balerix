//! Crew object cache and per-agent private clone (Spec N; Phase 2 spec
//! §4.2 step 1 before it).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use balerix_core::{MaterializeError, RepoRef};

use crate::fsutil::write_atomic;
use crate::home::render_hosts_yml;
use crate::launch::outer_path;
use crate::layout::{AgentPaths, CrewPaths};
use crate::quote::sh_quote;
use crate::sandbox::write_git_profile;
use crate::socket_policy::{SocketPolicy, wrap_command};
use crate::tools::{Cmd, CmdOutput, ToolPaths};

/// The environment of every git call balerix makes. `git` honours
/// `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_PREFIX` and
/// `GIT_COMMON_DIR` from the environment over an explicit `-C`: if any of
/// these leak in (a pre-commit hook exports them, and so does a daemon
/// started under one), every `-C` call would silently operate on whatever
/// repository those variables name; scrubbed. `GIT_NO_LAZY_FETCH=1` is
/// set, over whatever the daemon inherited: a `0` in its environment
/// would otherwise reach the harvest's `upload-pack` (#67). Harmless on
/// the cache and a fresh clone, which have no promisor remote; what it
/// guards is described at `PROMISOR_KEYS`.
pub(crate) fn scrub_git_env(mut cmd: Cmd) -> Cmd {
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_PREFIX",
        "GIT_COMMON_DIR",
    ] {
        cmd = cmd.env_remove(var);
    }
    cmd.env("GIT_NO_LAZY_FETCH", "1")
}

/// Config keys that declare a promisor remote, in the form `git config
/// --name-only` prints them (lowercased). `inspect.rs`'s `FILTER_KEYS`
/// carries the same alternation for the workspace reader. A promisor
/// remote in the clone's config would make any call that reads a missing
/// object (`status`, `diff`) fetch it from `remote.<x>.url`, and run
/// `remote.<x>.uploadpack` on the way. `GIT_NO_LAZY_FETCH=1` (the git
/// profile's `set_vars`, and `scrub_git_env`) closes that from git 2.45.1
/// (and the patched maintenance releases from 2.39.4); the git profile's
/// blocked network is what holds on any git, and the refusal by key
/// (`check_clone_config`, the workspace reader's filter probe) is the
/// operator's message
/// (#67).
pub(crate) const PROMISOR_KEYS: &str = r"^(extensions\.partialclone|remote\..*\.promisor)$";

/// What to do with an existing clone (Spec N §4 step 1): Spec L §12's
/// marker rule, as a function of the marker, the clone's HEAD (`None`
/// when detached) and the configured `branch`. `dirty` is consulted only
/// when the clone would have to move, so the git call behind it is not
/// made on the common path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloneDecision {
    /// The marker matches, or HEAD is detached: leave the clone as it is.
    Reuse,
    /// HEAD already sits on `branch`: write the marker, move nothing.
    Record,
    /// A clean clone on another branch: harvest `old`, delete, re-create.
    Recreate { old: String },
    /// A dirty clone on another branch: fail, naming `old`.
    Dirty { old: String },
}

pub fn decide_clone<E>(
    marker: Option<&str>,
    head: Option<&str>,
    branch: &str,
    dirty: impl FnOnce() -> Result<bool, E>,
) -> Result<CloneDecision, E> {
    let marker = marker.map(str::trim);
    if marker == Some(branch) {
        return Ok(CloneDecision::Reuse);
    }
    let Some(head) = head.map(str::trim) else {
        return Ok(CloneDecision::Reuse);
    };
    if head == branch {
        return Ok(CloneDecision::Record);
    }
    let old = marker.unwrap_or(head).to_string();
    Ok(if dirty()? {
        CloneDecision::Dirty { old }
    } else {
        CloneDecision::Recreate { old }
    })
}

/// What `Workspace::settle_clone` found of an existing clone, and what
/// its caller does next. It writes nothing outside the cache: removing
/// the clone and writing the marker are the caller's, so a host removes
/// the directory and a Job empties its mount (Spec O §27).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// The clone stays: its marker is the branch, or HEAD is detached.
    Kept,
    /// The clone stays and HEAD is already on the branch: the marker is
    /// to be written (the pod's `ensure_clone` writes it).
    Record,
    /// There is no clone (`workspace/.git` absent): whatever is left in
    /// `workspace/` is a half-made clone to remove, and one is made.
    NoClone,
    /// The clone was on `old`, and `old` was fetched into the cache
    /// (`harvested`; false when the clone no longer had that branch):
    /// the clone is to be removed.
    Remove { old: String, harvested: bool },
    /// A clone on `old` with local changes: refused, in the caller's
    /// words, since the remedy differs on a host and in a pod.
    Dirty { old: String },
    /// `workspace/.git` is a file: a 0.1 worktree, refused likewise.
    Worktree,
}

/// The refusal of a dirty clone whose branch changed, without a remedy:
/// the caller adds the one that fits a host or a pod.
pub fn dirty_clone(old: &str, branch: &str) -> String {
    format!(
        "the clone was created on branch {old:?} but the agent's branch is {branch:?}, \
         and the clone has local changes"
    )
}

/// Whether an exit code `agent_git` accepted is git's own answer. The
/// yes/no probes (`config --get-regexp`, `rev-parse --verify --quiet`,
/// `symbolic-ref -q`) print nothing when they exit 1. nono's own failure
/// also exits 1, after the canary as well as before it, and prints
/// `nono: …`; read as "no", that skips the harvest and the clone is
/// deleted unharvested (#109). So a non-zero exit that printed anything
/// is a failure. That fails closed for a git warning beside a real "no"
/// too, as any git error does (#74). Exit 0 is not inspected.
fn is_gits_answer(out: &CmdOutput) -> bool {
    out.code == 0 || out.stderr.trim().is_empty()
}

/// `remove_dir_all` that treats a missing path as done. Not `Runtime::rm_rf`:
/// that one waits out nono's ledger writes, and no process writes a clone
/// while the daemon materializes or removes it.
fn remove_tree(id: &str, path: &Path) -> Result<(), MaterializeError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(MaterializeError::Io {
            id: id.to_string(),
            path: path.to_path_buf(),
            message: e.to_string(),
        }),
    }
}

/// The error to return when a step failed half-way through making the
/// repository at `path`: `failed` once the remains are removed, or, when
/// the removal fails too, one naming both — a partial removal that left
/// `.git` behind would otherwise be judged a whole repository with every
/// file deleted on the next pass, with the real error never reported
/// (#74).
fn discard_half_made(id: &str, path: &Path, failed: MaterializeError) -> MaterializeError {
    match remove_tree(id, path) {
        Ok(()) => failed,
        // A mount point (the cache in a sync Job, Spec O §20.3) can be
        // emptied and never removed; empty is what the next pass needs.
        Err(_) if std::fs::read_dir(path).is_ok_and(|mut d| d.next().is_none()) => failed,
        Err(cleanup) => {
            let strip = |e: &MaterializeError| {
                let s = e.to_string();
                s.strip_prefix(&format!("{id}: "))
                    .map(str::to_string)
                    .unwrap_or(s)
            };
            MaterializeError::Invalid {
                id: id.to_string(),
                message: format!(
                    "{}; then removing what it left behind failed: {}",
                    strip(&failed),
                    strip(&cleanup)
                ),
            }
        }
    }
}

/// Refuses an agent's clone whose object store could be another
/// repository's, before the daemon runs any git in it or fetches from it
/// (Spec N §5, §12). The daemon reads every repository its uid can; the
/// agent reads only its own clone and the crew cache. A harvest is a
/// fetch the daemon runs, served by `upload-pack` in the clone, and it
/// writes whatever that serves into the crew cache, which every sibling
/// then reads: a confused deputy unless the clone's objects and refs are
/// the clone's own. The ways the agent could point them elsewhere, all
/// of them paths the daemon follows but the agent's sandbox never checks:
///
/// - `objects/info/alternates` naming another repository's objects (say
///   another crew's cache), with a hand-written `refs/heads/<assigned>`
///   holding a commit from it; so `alternates` must be exactly the one
///   line `clone --reference` wrote, the crew cache's canonical path;
/// - `.git` itself, `.git/objects`, a pack directory or a single pack
///   replaced by a symlink into another repository; so `.git` must be a
///   real directory and nothing under `.git/objects` may be a symlink;
/// - `.git/commondir`, which makes git read objects and refs from the
///   directory it names (the linked-worktree mechanism): a clone has none;
/// - an `objects/info/alternates` in the *cache*, which is cloned without
///   `--reference` and so never legitimately has one (a 0.1.x agent,
///   adopted under its old sandbox, could write the crew clone's).
///
/// The object store is pinned by this check. The clone's *config* is
/// checked by `Workspace::check_clone_config` before the first git call
/// that reads objects, not here: a matching marker reuses the clone with
/// no git call at all (Spec L §12).
///
/// A `.git` git no longer accepts as a repository is not checked here:
/// `agent_git` names it with `--git-dir` and the harvest fetches with
/// `upload-pack --strict`, so neither falls back to `workspace/` itself.
/// `GIT_CEILING_DIRECTORIES` bounds upward discovery only and helps with
/// none of this. The session is stopped before both callers run, but a
/// process the agent detached can outlive the tmux kill and rewrite the
/// clone after this check (#70). That is why the git calls after it run
/// under the git profile (`agent_git`, and the harvest's `upload-pack`):
/// this check names the problem for the operator, the profile is what
/// holds. Filesystem only: no git call.
fn check_clone(id: &str, crew: &CrewPaths, agent: &AgentPaths) -> Result<(), MaterializeError> {
    use std::os::unix::ffi::OsStrExt;

    let refuse = |path: &Path, found: &str, store: &str| refuse_clone(id, path, found, store);
    let dot_git = agent.workspace.join(".git");
    match std::fs::symlink_metadata(&dot_git) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(refuse(&dot_git, "not a real directory", CLONE_STORE)),
        Err(e) => return Err(refuse(&dot_git, &format!("unreadable ({e})"), CLONE_STORE)),
    }
    let commondir = dot_git.join("commondir");
    if std::fs::symlink_metadata(&commondir).is_ok() {
        return Err(refuse(
            &commondir,
            "present (a clone has none)",
            CLONE_STORE,
        ));
    }
    let cache_alternates = crew.cache_objects().join("info").join("alternates");
    if std::fs::symlink_metadata(&cache_alternates).is_ok() {
        return Err(refuse(
            &cache_alternates,
            "present (the cache is cloned without --reference)",
            CACHE_STORE,
        ));
    }
    let objects = dot_git.join("objects");
    match first_symlink(&objects) {
        Ok(None) => {}
        Ok(Some(link)) => return Err(refuse(&link, "a symlink", CLONE_STORE)),
        Err((path, e)) => return Err(refuse(&path, &format!("unreadable ({e})"), CLONE_STORE)),
    }
    let alternates = objects.join("info").join("alternates");
    let cache = crew
        .cache_objects()
        .canonicalize()
        .map_err(|e| MaterializeError::Io {
            id: id.to_string(),
            path: crew.cache_objects(),
            message: e.to_string(),
        })?;
    match std::fs::read(&alternates) {
        Ok(bytes) => {
            let line = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
            if line != cache.as_os_str().as_bytes() {
                return Err(refuse(
                    &alternates,
                    &format!(
                        "names something other than the crew cache {}",
                        cache.display()
                    ),
                    CLONE_STORE,
                ));
            }
        }
        Err(e) => {
            return Err(refuse(
                &alternates,
                &format!("unreadable ({e})"),
                CLONE_STORE,
            ));
        }
    }
    Ok(())
}

/// The wording of a `check_clone`/`check_clone_config` refusal: which
/// `store` is suspect (`CLONE_STORE` or `CACHE_STORE`), and the remedy.
fn refuse_clone(id: &str, path: &Path, found: &str, store: &str) -> MaterializeError {
    // `id` is `<fleet>/<crew>/<agent>`
    let fleet = id.split('/').next().unwrap_or(id);
    MaterializeError::Invalid {
        id: id.to_string(),
        message: format!(
            "{}: {found}, so {store}. Run `balerix down {fleet} --purge` to \
             delete it (push unpushed work first)",
            path.display()
        ),
    }
}
const CLONE_STORE: &str = "the clone's objects may be another repository's; \
                     balerix runs no git in it and harvests nothing from it";
const CACHE_STORE: &str = "the crew cache's objects may be another repository's; \
                     balerix runs no git in the clone and harvests nothing into the cache";

/// `path` as a `file://` URL that git decodes back to exactly its bytes:
/// every byte but an unreserved URL character or `/` is percent-encoded,
/// `%` itself included, so a `%20` in a path stays `%20`.
fn file_url(path: &Path) -> String {
    use std::fmt::Write;
    use std::os::unix::ffi::OsStrExt;

    let mut url = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            url.push(char::from(b));
        } else {
            let _ = write!(url, "%{b:02X}");
        }
    }
    url
}

/// The first symlink at or under `root`, found without following any.
/// A work list, not recursion: the agent decides how deep the tree goes.
fn first_symlink(root: &Path) -> Result<Option<PathBuf>, (PathBuf, std::io::Error)> {
    first_symlink_with(root, |p| std::fs::symlink_metadata(p))
}

/// `first_symlink` with the `lstat` given. An entry under `root` that is
/// gone by the time it is read is skipped (#136): git's own maintenance
/// removes its lock files mid-scan, and what no longer exists cannot be a
/// symlink. `root` itself missing is still an error.
fn first_symlink_with(
    root: &Path,
    mut lstat: impl FnMut(&Path) -> std::io::Result<std::fs::Metadata>,
) -> Result<Option<PathBuf>, (PathBuf, std::io::Error)> {
    let vanished =
        |path: &Path, e: &std::io::Error| path != root && e.kind() == std::io::ErrorKind::NotFound;
    let mut todo = vec![root.to_path_buf()];
    while let Some(path) = todo.pop() {
        let meta = match lstat(&path) {
            Ok(m) => m,
            Err(e) if vanished(&path, &e) => continue,
            Err(e) => return Err((path, e)),
        };
        if meta.file_type().is_symlink() {
            return Ok(Some(path));
        }
        if meta.is_dir() {
            let entries = match std::fs::read_dir(&path) {
                Ok(entries) => entries,
                Err(e) if vanished(&path, &e) => continue,
                Err(e) => return Err((path, e)),
            };
            for entry in entries {
                todo.push(entry.map_err(|e| (path.clone(), e))?.path());
            }
        }
    }
    Ok(None)
}

/// `/usr, /lib, /lib64 and /bin`: the git profile's system prefixes, as
/// the canary's hint names them (`/etc` is granted too, but holds no
/// library).
fn system_prefixes() -> String {
    let dirs: Vec<&str> = crate::sandbox::SYSTEM_READ
        .iter()
        .copied()
        .filter(|d| *d != "/etc")
        .collect();
    match dirs.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        _ => dirs.join(", "),
    }
}

/// `nono`'s arguments up to and including the `--` before the program,
/// for a call in `agent`'s existing clone: `-s --log-file
/// <logs>/nono-git.log run --no-audit --profile <git profile> --`. No audit
/// trail: one per call, at the web plugin's poll, would grow nono's state
/// without bound (the session record `--no-audit` keeps goes to a
/// `ScratchHome`). Under the Deny socket policy the program runs
/// through `<balerix> sandbox-exec --`, appended here. The profile is
/// `sandbox::render_git_profile`; `write_git_profile` must have run with
/// the same policy.
fn sandbox_args(agent: &AgentPaths, policy: SocketPolicy, balerix: &Path) -> Vec<String> {
    sandbox_args_writing(agent, policy, balerix, None)
}

/// `sandbox_args`, with `writable` granted read-write (`--allow`) on top
/// of the read-only profile: only the clone step's index refresh
/// (`Workspace::refresh_index`) passes one, its fresh clone's `.git`.
fn sandbox_args_writing(
    agent: &AgentPaths,
    policy: SocketPolicy,
    balerix: &Path,
    writable: Option<&Path>,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-s".into(),
        "--log-file".into(),
        agent.logs.join("nono-git.log").display().to_string(),
        "run".into(),
        "--no-audit".into(),
        "--profile".into(),
        agent.git_profile.display().to_string(),
    ];
    if let Some(dir) = writable {
        args.extend(["--allow".into(), dir.display().to_string()]);
    }
    args.push("--".into());
    args.extend(wrap_command(policy, balerix, Vec::new()));
    args
}

/// git and its options on every call in `agent`'s clone, before the
/// caller's: fsmonitor off, hooks pointed at an empty directory, and the
/// repository named exactly (`sandboxed_git`).
fn git_command(tools: &ToolPaths, crew: &CrewPaths, agent: &AgentPaths) -> Vec<String> {
    vec![
        tools.git.display().to_string(),
        "-c".into(),
        "core.fsmonitor=false".into(),
        "-c".into(),
        format!("core.hooksPath={}", crew.no_hooks().display()),
        "-C".into(),
        agent.workspace.display().to_string(),
        format!("--git-dir={}", agent.workspace.join(".git").display()),
        format!("--work-tree={}", agent.workspace.display()),
    ]
}

/// The nono command for one call under the git profile, from an empty
/// environment with a fresh `ScratchHome`, logged to the crew's `git.log`;
/// `writable` as `sandbox_args_writing`'s.
fn sandboxed_cmd(
    tools: &ToolPaths,
    policy: SocketPolicy,
    crew: &CrewPaths,
    agent: &AgentPaths,
    log: GitLog,
    writable: Option<&Path>,
) -> Result<(Cmd, ScratchHome), GitFailure> {
    let log_file = crew.logs.join("git.log");
    let home = ScratchHome::new(agent).map_err(|e| GitFailure {
        subcommand: String::new(),
        args: Vec::new(),
        stderr: format!(
            "cannot create nono's home beside {}: {e}",
            agent.nono_home.display()
        ),
        timed_out: None,
    })?;
    let cmd = Cmd::new(&tools.nono)
        .env_clear()
        .env("HOME", home.path().display().to_string())
        .env("PATH", outer_path(tools));
    let cmd = match log {
        GitLog::Full => cmd.log(&log_file),
        GitLog::ArgvOnly => cmd.log_argv_only(&log_file),
    }
    .args(sandbox_args_writing(
        agent,
        policy,
        &tools.balerix,
        writable,
    ));
    Ok((cmd, home))
}

/// nono's `$HOME` for one daemon call under the git profile: a fresh
/// directory beside the agent's `nono/`, removed when the guard drops,
/// success or failure. nono writes a session record under `$HOME` on every
/// `run` (`.local/state/nono/sessions/`), even with `--no-audit`; in the
/// agent's own `nono/`, where its live session's record also is, those
/// could not be swept without risking that one. nono writes it as the
/// supervisor, outside the sandbox, so the profile grants nothing on it;
/// git inside sees no `HOME` at all (`deny_vars`), as before, so no
/// global config either way.
pub(crate) struct ScratchHome(PathBuf);

impl ScratchHome {
    pub(crate) fn new(agent: &AgentPaths) -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = agent
            .nono_home
            .parent()
            .unwrap_or(&agent.nono_home)
            .to_path_buf();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default();
        let path = parent.join(format!(
            ".nono-git-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // `create_dir`, not `create_dir_all`: a name already there is
        // someone else's, never reused
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

/// Removes the `ScratchHome`s another process left beside `agent`'s
/// `nono/`: a daemon that was SIGKILLed mid-call never ran their guards.
/// Only one daemon runs against a state root, so a directory named for
/// another pid is never in use; this process's own are left alone.
pub(crate) fn sweep_stale_homes(agent: &AgentPaths) {
    let Some(parent) = agent.nono_home.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let ours = std::process::id().to_string();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(".nono-git-")) else {
            continue;
        };
        let pid = rest.split('-').next().unwrap_or_default();
        if pid != ours && entry.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

impl Drop for ScratchHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How far past the second of `written` a file's mtime must be for git
/// to take entries written up to then as clean (`Workspace::refresh_index`).
/// The kernel stamps files from a coarse clock that can trail the system
/// clock by a tick (10 ms at `HZ=100`); twice that keeps a write made just
/// after the boundary from landing in the old second.
const PAST_SECOND_MARGIN: Duration = Duration::from_millis(20);

/// The most `wait_past_second` waits: one second and the margin, whatever
/// `written` says (a clock that moved backwards, an mtime in the future).
const PAST_SECOND_MAX: Duration = Duration::from_millis(1100);

/// How long to wait, from `now`, until a file written then gets an mtime
/// in a later whole second than `written` (plus `PAST_SECOND_MARGIN`):
/// git's racy-clean check compares whole seconds unless it was built with
/// `USE_NSEC`, so the next second is enough for either build.
pub(crate) fn wait_past_second(written: SystemTime, now: SystemTime) -> Duration {
    let second = written
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let target = UNIX_EPOCH + Duration::from_secs(second + 1) + PAST_SECOND_MARGIN;
    target
        .duration_since(now)
        .unwrap_or_default()
        .min(PAST_SECOND_MAX)
}

/// The most the clone step's index refresh may take
/// (`Workspace::refresh_index`): far more than re-hashing a large
/// checkout needs, so a hung start cannot hold the clone step.
const INDEX_REFRESH_DEADLINE: Duration = Duration::from_secs(60);

/// What `sandboxed_git` writes to the crew's `git.log`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitLog {
    /// argv, stdout, stderr and the exit status.
    Full,
    /// argv, stderr and the exit status: for a call whose stdout is a
    /// request's payload (the workspace reader's diffs).
    ArgvOnly,
}

/// A failed `sandboxed_git`, reported as git's: git's subcommand (its
/// first argument past the options), and the argv nono ran, or git's own
/// arguments when the call exited with an accepted non-zero code but
/// printed on stderr (`is_gits_answer`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitFailure {
    pub subcommand: String,
    pub args: Vec<String>,
    pub stderr: String,
    /// The call's `deadline`, when it was ended by it.
    pub timed_out: Option<Duration>,
}

/// One git call inside the agent's existing clone, run under the git
/// profile (`sandbox::render_git_profile`; Spec N amendment 2026-10-01,
/// #68, #70): it reads the clone and the crew cache's objects, writes
/// nothing and has no network, so whatever the clone points at, git sees
/// no more than the agent could. The checks before it give the operator a
/// readable refusal; the profile is the boundary, and holds even when the
/// clone changes after the checks.
///
/// nono starts from an empty environment (`HOME` is the agent's `nono/`,
/// as for `launch.sh`), and the profile's `set_vars` carry the hardening
/// (`GIT_OPTIONAL_LOCKS=0`, `GIT_NO_LAZY_FETCH=1`, no prompt, no system
/// config, the ceiling at the agent's root); the `-c` pairs turn off
/// fsmonitor and point hooks at an empty directory, over any config the
/// agent wrote. `--git-dir` names `workspace/.git` exactly: with `-C`
/// alone, a `.git` git rejects (say, its `HEAD` deleted) makes git take
/// `workspace/` itself for a bare repository. `accepted` are the exit
/// codes that count as success (0 included). A failure is reported as
/// git's, with git's subcommand, not nono's. An accepted non-zero exit
/// that printed anything on stderr is a failure too (`is_gits_answer`).
/// With a `deadline`, the call's whole tree is ended once it passes
/// (`Cmd::timeout`) and the failure says so (`timed_out`); the
/// `ScratchHome` goes either way. `write_git_profile` must have run.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sandboxed_git(
    tools: &ToolPaths,
    policy: SocketPolicy,
    crew: &CrewPaths,
    agent: &AgentPaths,
    args: &[&str],
    accepted: &[i32],
    log: GitLog,
    deadline: Option<Duration>,
) -> Result<CmdOutput, GitFailure> {
    let (cmd, _home) = sandboxed_cmd(tools, policy, crew, agent, log, None)?;
    let mut cmd = cmd
        .args(git_command(tools, crew, agent))
        .args(args.iter().copied());
    if let Some(d) = deadline {
        cmd = cmd.timeout(d);
    }
    // git's own: the first word that is neither an option nor a `-c` value
    let mut words = args.iter().copied();
    let mut subcommand = String::new();
    while let Some(word) = words.next() {
        if word == "-c" {
            words.next();
        } else if !word.starts_with('-') {
            subcommand = word.to_string();
            break;
        }
    }
    let out = cmd.run_with_exit_codes(accepted).map_err(|f| GitFailure {
        subcommand: subcommand.clone(),
        args: f.args,
        stderr: f.stderr,
        timed_out: f.timed_out,
    })?;
    if !is_gits_answer(&out) {
        return Err(GitFailure {
            subcommand,
            args: args.iter().map(|a| (*a).to_string()).collect(),
            stderr: out.stderr.clone(),
            timed_out: None,
        });
    }
    Ok(out)
}

/// Where a streamed call's stdout goes, chunk by chunk.
pub(crate) type StdoutSink<'a> = &'a mut dyn FnMut(&[u8]);

/// Several git calls in one sandbox start (#174): `script`, a fixed
/// constant, runs as `/bin/sh -c <script> sh <leading…> <git…>` under the
/// git profile (`/bin` is one of its system prefixes), where `<git…>` is
/// git with `sandboxed_git`'s options and then `git_args`, so the script
/// runs each call as `"$@" <subcommand> …` after shifting `leading` off.
/// Every value reaches the script as an argument, never as script text.
/// Logged as one argv-only `git.log` entry. `accepted` are the script's
/// exit codes, which the caller interprets: no `is_gits_answer` here, since
/// a nono that fails exits with its own code, not one of the script's. A
/// failure is reported with the subcommand `sh`. With a `sink`, stdout
/// goes to it as it arrives (`Cmd::run_streaming`) instead of into the
/// output. The start, and every process in it, is ended at `deadline`
/// (`sandboxed_git`'s). `write_git_profile` must have run.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sandboxed_git_script(
    tools: &ToolPaths,
    policy: SocketPolicy,
    crew: &CrewPaths,
    agent: &AgentPaths,
    script: &'static str,
    leading: &[&str],
    git_args: &[&str],
    accepted: &[i32],
    sink: Option<StdoutSink<'_>>,
    deadline: Duration,
) -> Result<CmdOutput, GitFailure> {
    let (cmd, _home) = sandboxed_cmd(tools, policy, crew, agent, GitLog::ArgvOnly, None)?;
    let cmd = cmd
        .args(["/bin/sh", "-c", script, "sh"])
        .args(leading.iter().copied())
        .args(git_command(tools, crew, agent))
        .args(git_args.iter().copied())
        .timeout(deadline);
    match sink {
        Some(sink) => cmd.run_streaming(accepted, sink),
        None => cmd.run_with_exit_codes(accepted),
    }
    .map_err(|f| GitFailure {
        subcommand: "sh".into(),
        args: f.args,
        stderr: f.stderr,
        timed_out: f.timed_out,
    })
}

/// The most `sandbox_starts` waits for `/bin/true` on the clone step's
/// path; the workspace reader passes what is left of its budget (#213).
const START_CHECK_DEADLINE: Duration = Duration::from_secs(10);

/// Whether `/bin/true` runs under `agent`'s git profile: asked once a git
/// call under it has failed in a way that says git could not run, to tell
/// a git the profile cannot run (`git_read_hint`) from a sandbox that
/// does not start at all. One that has not answered within `deadline`
/// did not start.
fn sandbox_starts(
    tools: &ToolPaths,
    policy: SocketPolicy,
    agent: &AgentPaths,
    deadline: Duration,
) -> bool {
    let mut args = sandbox_args(agent, policy, &tools.balerix);
    args.push("/bin/true".into());
    let Ok(home) = ScratchHome::new(agent) else {
        return false;
    };
    Cmd::new(&tools.nono)
        .env_clear()
        .env("HOME", home.path().display().to_string())
        .env("PATH", outer_path(tools))
        .args(args)
        .timeout(deadline)
        .run()
        .is_ok()
}

/// Where a failed git call under the git profile left its detail: the
/// crew's `git.log` (git's stderr and exit status) and the agent's
/// `nono-git.log` (nono's own).
fn see_logs(crew: &CrewPaths, agent: &AgentPaths) -> String {
    format!(
        "see {} and {}",
        crew.logs.join("git.log").display(),
        agent.logs.join("nono-git.log").display()
    )
}

/// The hint for a git that could not run under the git profile (#111,
/// #214), typically one that loads its libraries from its own prefix:
/// `Some` when the sandbox itself starts (`sandbox_starts`), naming
/// `sandbox.git_read` and both logs; `None` when it does not, and the
/// hint would point the wrong way. A prefix, not a line: the error
/// displays its first line.
pub(crate) fn git_read_hint(
    tools: &ToolPaths,
    policy: SocketPolicy,
    crew: &CrewPaths,
    agent: &AgentPaths,
    deadline: Duration,
) -> Option<String> {
    sandbox_starts(tools, policy, agent, deadline).then(|| {
        format!(
            "git could not run under the git profile (the sandbox itself starts); if it \
             loads libraries from outside {}, add that prefix to `sandbox.git_read` in the \
             daemon's config.toml; {}",
            system_prefixes(),
            see_logs(crew, agent)
        )
    })
}

pub struct Workspace<'a> {
    pub tools: &'a ToolPaths,
    /// `GH_CONFIG_DIR` for the daemon's git calls when `git.auth: gh`.
    pub gh_config_dir: Option<PathBuf>,
    /// Spec O §8.1: the cache is a read-only mount kept current by a Job;
    /// a new clone skips the fetch into it and the harvested-branch seed.
    pub cache_is_read_only: bool,
    /// The daemon's `[sandbox] git_read` (#111): read-only prefixes the
    /// git profile grants beside the system ones. Empty in a pod.
    pub git_read: &'a [PathBuf],
    /// The socket policy the git profile and its calls run under (spec
    /// 2026-10-08): the `Runtime`'s.
    pub socket_policy: SocketPolicy,
}

impl Workspace<'_> {
    pub fn write_fleet_gh_config(
        dir: &Path,
        token: &str,
        id: &str,
    ) -> Result<(), MaterializeError> {
        let hosts = dir.join("hosts.yml");
        write_atomic(&hosts, render_hosts_yml(token).as_bytes(), 0o600).map_err(|e| {
            MaterializeError::Io {
                id: id.to_string(),
                path: hosts,
                message: e.to_string(),
            }
        })
    }

    /// One git call as the daemon, logged to the crew's `git.log`, with
    /// the gh credential helper when `git.auth: gh` (`scrub_git_env` on
    /// every call). For the cache and for a clone at creation; a call in
    /// an existing clone goes through `agent_git`.
    fn git(&self, id: &str, crew: &CrewPaths, args: &[&str]) -> Result<String, MaterializeError> {
        self.git_accepting(id, crew, args, &[0]).map(|o| o.stdout)
    }

    /// `git` as a yes/no question: exit 0 is `true`, exit 1 is `false`
    /// (`rev-parse --verify --quiet`, `merge-base --is-ancestor`), and any
    /// other exit is the error it is, never a "no" (#74).
    fn git_probe(
        &self,
        id: &str,
        crew: &CrewPaths,
        args: &[&str],
    ) -> Result<bool, MaterializeError> {
        self.git_accepting(id, crew, args, &[0, 1])
            .map(|o| o.code == 0)
    }

    /// `git`, with `accepted` the exit codes that count as success.
    fn git_accepting(
        &self,
        id: &str,
        crew: &CrewPaths,
        args: &[&str],
        accepted: &[i32],
    ) -> Result<CmdOutput, MaterializeError> {
        let mut cmd = scrub_git_env(Cmd::new(&self.tools.git).log(&crew.logs.join("git.log")));
        if let Some(dir) = &self.gh_config_dir {
            cmd = cmd.env("GH_CONFIG_DIR", dir.display().to_string()).args([
                "-c",
                "credential.helper=",
                "-c",
                &format!(
                    "credential.helper=!{} auth git-credential",
                    self.tools.gh.display()
                ),
            ]);
        }
        cmd = cmd
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(args.iter().copied());
        cmd.run_with_exit_codes(accepted)
            .map_err(|f| MaterializeError::Tool {
                id: id.to_string(),
                tool: f.tool,
                subcommand: f.subcommand,
                args: f.args,
                stderr: f.stderr,
            })
    }

    /// The crew's object cache (Spec N §3): a `--no-checkout` clone, made
    /// when absent, with `gc.auto=0` so that git never gcs it on its own —
    /// a clone borrowing objects from it is only safe while the cache
    /// never loses one — and with its HEAD detached and the clone-time
    /// default branch deleted (`detach_cache_head`). A cache that exists
    /// is left alone, so a steady-state pass costs no git call (Phase 3
    /// spec §6.1); `ensure_clone` fetches when it actually needs
    /// `origin/<ref>`. Whatever fails after the clone removes it, so the
    /// next pass clones and pins again.
    pub fn ensure_repo(
        &self,
        id: &str,
        crew: &CrewPaths,
        repo: &RepoRef,
        git_ref: &str,
    ) -> Result<(), MaterializeError> {
        let _ = git_ref;
        if crew.repo.join(".git").is_dir() {
            return Ok(());
        }
        std::fs::create_dir_all(&crew.root).map_err(|e| MaterializeError::Io {
            id: id.to_string(),
            path: crew.root.clone(),
            message: e.to_string(),
        })?;
        let cache = crew.repo.display().to_string();
        self.git(
            id,
            crew,
            &[
                "clone",
                "--quiet",
                "--no-checkout",
                &repo.clone_url(),
                &cache,
            ],
        )?;
        // A cache left behind with its pin or its HEAD not yet settled
        // would be taken for a whole one on every later pass (#74).
        self.git(id, crew, &["-C", &cache, "config", "gc.auto", "0"])
            .and_then(|_| self.detach_cache_head(id, crew))
            .map_err(|e| discard_half_made(id, &crew.repo, e))
    }

    /// The sync Job's cache step (Spec O §8.3): the cache made when absent,
    /// then fetched, since in a pod nothing else ever fetches it
    /// (`create_clone` skips its own fetch there). Creates the crew's
    /// `no-hooks` directory, which the pod's git calls name and cannot
    /// make on a read-only mount. Returns the commit `origin/<git_ref>` is
    /// at, the Crew's `cacheRef`.
    pub fn sync_cache(
        &self,
        id: &str,
        crew: &CrewPaths,
        repo: &RepoRef,
        git_ref: &str,
    ) -> Result<String, MaterializeError> {
        self.ensure_repo(id, crew, repo, git_ref)?;
        let no_hooks = crew.no_hooks();
        std::fs::create_dir_all(&no_hooks).map_err(|e| MaterializeError::Io {
            id: id.to_string(),
            path: no_hooks,
            message: e.to_string(),
        })?;
        let cache = crew.repo.display().to_string();
        self.git(
            id,
            crew,
            &[
                "-C",
                &cache,
                "fetch",
                "--quiet",
                "--no-auto-gc",
                "--prune",
                "origin",
            ],
        )?;
        let remote = format!("refs/remotes/origin/{git_ref}^{{commit}}");
        // `--quiet` answers a missing ref with exit 1 and no text
        if !self.git_probe(
            id,
            crew,
            &["-C", &cache, "rev-parse", "--verify", "--quiet", &remote],
        )? {
            return Err(MaterializeError::Invalid {
                id: id.to_string(),
                message: format!("the remote has no branch {git_ref}"),
            });
        }
        self.git(id, crew, &["-C", &cache, "rev-parse", "--verify", &remote])
            .map(|sha| sha.trim().to_string())
    }

    /// A `--no-checkout` clone still has the remote's default branch under
    /// `refs/heads`, with HEAD on it. That copy is the tip at clone time,
    /// which `fetch` never moves, so once origin force-pushes past it the
    /// seed in `create_clone` would take it for a harvest holding commits
    /// origin lacks (#69). The cache has no working tree for HEAD to
    /// serve: detach it and delete the branch, so that everything under
    /// the cache's `refs/heads` is a harvest, and no fetch into the cache
    /// ever meets the branch HEAD names. An unborn HEAD (an empty remote)
    /// has no branch to delete.
    fn detach_cache_head(&self, id: &str, crew: &CrewPaths) -> Result<(), MaterializeError> {
        let cache = crew.repo.display().to_string();
        let head = self.git(id, crew, &["-C", &cache, "symbolic-ref", "HEAD"])?;
        let head = head.trim();
        let sha = self.git(
            id,
            crew,
            &["-C", &cache, "for-each-ref", "--format=%(objectname)", head],
        )?;
        let sha = sha.trim();
        if sha.is_empty() {
            return Ok(());
        }
        self.git(
            id,
            crew,
            &["-C", &cache, "update-ref", "--no-deref", "HEAD", sha],
        )?;
        self.git(id, crew, &["-C", &cache, "update-ref", "-d", head])?;
        Ok(())
    }

    /// A promisor remote in the clone's config (`extensions.partialClone`,
    /// or any `remote.<x>.promisor`: `PROMISOR_KEYS`) would make the first
    /// git call that reads a missing object (`status`, the harvest's
    /// `upload-pack`) fetch it from `remote.<x>.url` as the daemon, into
    /// the object store `check_clone` pinned, and run
    /// `remote.<x>.uploadpack` on the way. Refused by key, before that
    /// call, on any git version (#67); `GIT_NO_LAZY_FETCH=1` on every call
    /// closes the same route on a git that honours it (2.45.1, or a
    /// patched maintenance release from 2.39.4) and stays as the second
    /// layer. The probe is `config --local --includes` through
    /// `agent_git`, the workspace reader's filter probe (`PROBE`): it
    /// reads no object, so it is safe to run before its own verdict, and
    /// it follows `include.path` and `includeIf` exactly as git would in
    /// that repository, which a read of the file alone would not. Runs
    /// after `check_clone`, never before it.
    fn check_clone_config(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
    ) -> Result<(), MaterializeError> {
        let keys = self.agent_git(
            id,
            crew,
            agent,
            &[
                "config",
                "--local",
                "--includes",
                "--name-only",
                "--get-regexp",
                PROMISOR_KEYS,
            ],
            &[0, 1],
        )?;
        match keys.lines().next().map(str::trim).filter(|k| !k.is_empty()) {
            Some(key) => Err(refuse_clone(
                id,
                &agent.workspace.join(".git").join("config"),
                &format!("sets {key} (a promisor remote)"),
                CLONE_STORE,
            )),
            None => Ok(()),
        }
    }

    /// Writes the git profile and proves the sandbox starts under it,
    /// before any probe's exit code is trusted. The yes/no probes after
    /// this (`config --get-regexp`, `rev-parse --verify --quiet`) accept
    /// exit 1 as git's answer, and nono's own failure to run also exits 1;
    /// a `version` that must exit 0 tells the two apart for a nono that
    /// validates but cannot run at all, and names the log to read. A
    /// failure on a later call is caught by `is_gits_answer`. When
    /// `version` fails, `/bin/true` under the same profile says whether
    /// the sandbox started at all, and the error names `sandbox.git_read`
    /// when it did (#111).
    fn prepare_sandbox(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
    ) -> Result<(), MaterializeError> {
        write_git_profile(
            self.tools,
            id,
            agent,
            crew,
            self.git_read,
            self.socket_policy,
        )?;
        self.agent_git(id, crew, agent, &["version"], &[0])
            .map(|_| ())
            .map_err(|e| match e {
                MaterializeError::Tool {
                    id,
                    tool,
                    subcommand,
                    args,
                    stderr,
                } => {
                    // #111: a sandbox that runs `/bin/true` started; it is
                    // git that could not run under it
                    let hint = git_read_hint(
                        self.tools,
                        self.socket_policy,
                        crew,
                        agent,
                        START_CHECK_DEADLINE,
                    )
                    .unwrap_or_else(|| {
                        format!("the sandbox did not start; {}", see_logs(crew, agent))
                    });
                    MaterializeError::Tool {
                        id,
                        tool,
                        subcommand,
                        args,
                        stderr: format!("{hint}: {stderr}"),
                    }
                }
                other => other,
            })
    }

    /// One git call inside the agent's existing clone, under the git
    /// profile (`sandboxed_git`), as a materialize step.
    fn agent_git(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        args: &[&str],
        accepted: &[i32],
    ) -> Result<String, MaterializeError> {
        sandboxed_git(
            self.tools,
            self.socket_policy,
            crew,
            agent,
            args,
            accepted,
            GitLog::Full,
            None,
        )
        .map(|o| o.stdout)
        .map_err(|f| MaterializeError::Tool {
            id: id.to_string(),
            tool: "git".into(),
            subcommand: f.subcommand,
            args: f.args,
            stderr: f.stderr,
        })
    }

    /// The agent's private clone on `branch` (Spec N §4): a clone of
    /// `repo` made with `--reference` to the crew cache, so objects the
    /// cache holds are neither transferred nor duplicated.
    ///
    /// An existing clone (`workspace/.git` a directory) is judged by
    /// `decide_clone`, Spec L §12's marker rule unchanged: a matching
    /// marker reuses the clone whatever its HEAD (#60); a clone without a
    /// marker is judged by HEAD once; a detached HEAD is reused as is; a
    /// changed `branch` re-creates a clean clone after harvesting the old
    /// branch into the cache (§5), and fails a dirty one naming both
    /// branches. Every git call on an existing clone is `agent_git` (#62),
    /// none is made before `check_clone` has passed it, and none that
    /// reads an object before `check_clone_config` has (#67).
    ///
    /// A `workspace/.git` *file* is a worktree from balerix 0.1, refused
    /// with the remedy (N-6). A `workspace/` with no `.git` at all (a
    /// clone that crashed half-way) is replaced.
    ///
    /// A new clone starts with a fetch in the cache — the one moment
    /// `origin/<start_ref>` must be current, and what makes the clone
    /// cheap — then seeds `branch` from the cache's harvested copy when
    /// there is one holding commits `origin/<branch>` lacks, else creates
    /// it from `origin/<branch>` (a copy origin contains) or
    /// `origin/<start_ref>` (no copy). A seeded
    /// branch tracks `origin/<branch>` when the remote has it, as a branch
    /// created from it would. Whatever fails after `git clone` removes the
    /// half-made clone: a `--no-checkout` clone left behind would be
    /// judged an existing, dirty clone on the next pass.
    pub fn ensure_clone(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        repo: &RepoRef,
        branch: &str,
        start_ref: &str,
    ) -> Result<(), MaterializeError> {
        match self.settle_clone(id, crew, agent, branch)? {
            Settled::Kept => return Ok(()),
            Settled::Record => return Self::record_branch(id, agent, branch),
            Settled::Dirty { old } => {
                return Err(MaterializeError::Invalid {
                    id: id.to_string(),
                    message: format!(
                        "{}; commit or discard them in {} first",
                        dirty_clone(&old, branch),
                        agent.workspace.display()
                    ),
                });
            }
            Settled::Worktree => {
                // `id` is `<fleet>/<crew>/<agent>`
                let fleet = id.split('/').next().unwrap_or(id);
                return Err(MaterializeError::Invalid {
                    id: id.to_string(),
                    message: format!(
                        "{}: created by balerix 0.1 as a worktree; run `balerix down {fleet} \
                         --purge` and `up` again (push unpushed work first)",
                        agent.workspace.display()
                    ),
                });
            }
            Settled::Remove { .. } | Settled::NoClone => remove_tree(id, &agent.workspace)?,
        }
        self.create_clone(id, crew, agent, repo, branch, start_ref)
            .map_err(|e| discard_half_made(id, &agent.workspace, e))?;
        self.refresh_index(id, crew, agent);
        Self::record_branch(id, agent, branch)
    }

    /// A new clone's index, rewritten once the clock has left the second
    /// its checkout wrote it in (#253). The checkout writes the files and
    /// then the index within the same second, and git's racy-clean rule
    /// treats an entry whose file mtime is not older than the index's own
    /// (compared in whole seconds, or in nanoseconds when git is built
    /// with `USE_NSEC`) as possibly modified: every `status` re-reads
    /// those files. The workspace reader runs with `GIT_OPTIONAL_LOCKS=0`
    /// and never writes the result back, so every poll paid that until the
    /// agent's own git wrote the index. `update-index --refresh` re-hashes
    /// them once, here, and writes an index younger than every file it
    /// checked; the reader's `status` then trusts the stat data.
    ///
    /// Under the git profile like every call in the clone, with the
    /// clone's `.git` granted read-write for this one start: the profile
    /// is read-only, and `index.lock` and its rename need the directory.
    /// The profile and the command are made before the wait, which they
    /// overlap. No canary (`prepare_sandbox`): nothing here is a yes/no
    /// answer a nono failure could be mistaken for, and every failure is
    /// the same. No filter probe (the reader's `FILTER_KEYS`) before the
    /// re-hash either: its answer is always "no" in a fresh clone, whose
    /// only config is the one `git clone` wrote and whose git reads no
    /// system or global config. It is an optimisation, so a failure, or a
    /// start still running at `INDEX_REFRESH_DEADLINE`, is a warning in
    /// the crew's `git.log` and the clone stands; the reads are slower,
    /// not wrong. Nothing else runs in the clone yet, so an `index.lock`
    /// a failed start left is removed: the agent's own git would refuse
    /// to run beside it.
    fn refresh_index(&self, id: &str, crew: &CrewPaths, agent: &AgentPaths) {
        let dot_git = agent.workspace.join(".git");
        let refreshed = std::fs::symlink_metadata(dot_git.join("index"))
            .and_then(|m| m.modified())
            .map_err(|e| format!("cannot read the index's mtime: {e}"))
            .and_then(|written| {
                write_git_profile(
                    self.tools,
                    id,
                    agent,
                    crew,
                    self.git_read,
                    self.socket_policy,
                )
                .map_err(|e| e.to_string())?;
                let (cmd, _home) = sandboxed_cmd(
                    self.tools,
                    self.socket_policy,
                    crew,
                    agent,
                    GitLog::Full,
                    Some(&dot_git),
                )
                .map_err(|f| f.stderr)?;
                let cmd = cmd
                    .args(git_command(self.tools, crew, agent))
                    .args(["update-index", "-q", "--refresh"])
                    .timeout(INDEX_REFRESH_DEADLINE);
                std::thread::sleep(wait_past_second(written, SystemTime::now()));
                cmd.run().map(|_| ()).map_err(|f| match f.timed_out {
                    Some(d) => format!("timed out after {}s: {}", d.as_secs(), f.stderr),
                    None => f.stderr,
                })
            });
        if let Err(error) = refreshed {
            let lock = dot_git.join("index.lock");
            let removed = match std::fs::remove_file(&lock) {
                Ok(()) => format!(" (removed the {} it left)", lock.display()),
                Err(_) => String::new(),
            };
            let error = error.trim();
            tracing::warn!(agent = id, error, "index refresh after the clone failed");
            let log = crew.logs.join("git.log");
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)
            {
                use std::io::Write as _;
                let _ = writeln!(
                    f,
                    "# warning: {id}: index refresh after the clone failed, so reads \
                     re-hash the files the checkout wrote until the index is next \
                     written{removed}: {error}"
                );
            }
        }
    }

    /// `ensure_clone`'s judgement of an existing clone for `branch`
    /// (Spec N §4 step 1, Spec L §12's marker rule), without the new
    /// clone: a clean clone on another branch has its old branch
    /// harvested into the cache (`Remove`); a dirty one is `Dirty`; any
    /// other is kept. It removes nothing and writes no marker (see
    /// `Settled`). The branch-change Job (Spec O §27) runs it with the
    /// cache writable and the clone alone writable, so that the pod's
    /// `ensure_clone`, whose cache is read-only, finds no clone to move.
    pub fn settle_clone(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        branch: &str,
    ) -> Result<Settled, MaterializeError> {
        let dot_git = agent.workspace.join(".git");
        if dot_git.is_dir() {
            check_clone(id, crew, agent)?;
            let marker = std::fs::read_to_string(agent.branch_marker()).ok();
            if marker.as_deref().map(str::trim) == Some(branch) {
                // Spec L §12: marker equals `branch` → done, with no git
                // call; `decide_clone` agrees (`Reuse`)
                return Ok(Settled::Kept);
            }
            self.prepare_sandbox(id, crew, agent)?;
            self.check_clone_config(id, crew, agent)?;
            let head = self.head_branch(id, crew, agent)?;
            let decision = decide_clone(marker.as_deref(), head.as_deref(), branch, || {
                self.agent_git(id, crew, agent, &["status", "--porcelain"], &[0])
                    .map(|s| !s.trim().is_empty())
            })?;
            Ok(match decision {
                CloneDecision::Reuse => Settled::Kept,
                CloneDecision::Record => Settled::Record,
                CloneDecision::Dirty { old } => Settled::Dirty { old },
                CloneDecision::Recreate { old } => {
                    let harvested = self.harvest(id, crew, agent, &old, || {})?;
                    Settled::Remove { old, harvested }
                }
            })
        } else if dot_git.exists() {
            Ok(Settled::Worktree)
        } else {
            Ok(Settled::NoClone)
        }
    }

    /// Spec N §4 step 3. These calls run in a clone the agent has never
    /// touched, so they need no hardening; they go through `git` for the
    /// credential helper the clone needs.
    ///
    /// A `--no-checkout` clone already has the remote's default branch as
    /// a local branch with HEAD on it, so an agent assigned that branch
    /// needs three things another branch does not. The seed fetch carries
    /// `--update-head-ok`, since git refuses to fetch into the branch HEAD
    /// names and the fresh clone has no checkout or index to disturb. Its
    /// refspec is forced (`+`), since the fresh clone's copy is origin's
    /// tip and the harvested one may have diverged from it; the fresh copy
    /// is worth nothing. The create path is `checkout -B`, since `-b` fails
    /// on the branch that exists, for the same reason.
    ///
    /// A cache copy of `branch` that `origin/<branch>` already contains is
    /// not a seed: the branch is created from `origin/<branch>` instead,
    /// which is newer and carries everything the copy did.
    fn create_clone(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        repo: &RepoRef,
        branch: &str,
        start_ref: &str,
    ) -> Result<(), MaterializeError> {
        let cache = crew.repo.display().to_string();
        let ws = agent.workspace.display().to_string();
        if !self.cache_is_read_only {
            self.git(
                id,
                crew,
                &["-C", &cache, "fetch", "--quiet", "--no-auto-gc", "origin"],
            )?;
        }
        self.git(
            id,
            crew,
            &[
                "clone",
                "--quiet",
                "--no-checkout",
                "--reference",
                &cache,
                &repo.clone_url(),
                &ws,
            ],
        )?;
        let refname = format!("refs/heads/{branch}");
        let remote = format!("refs/remotes/origin/{branch}");
        // A pod's cache slice holds objects only: no harvested branch is
        // visible through it, and `rev-parse` there is "not a repository".
        let cached = !self.cache_is_read_only
            && self.git_probe(
                id,
                crew,
                &["-C", &cache, "rev-parse", "--verify", "--quiet", &refname],
            )?;
        // The cache's copy is a harvest (`ensure_repo` keeps no other
        // branch there), worth seeding from only while it holds commits
        // `origin/<branch>` lacks: one the agent pushed is behind origin's
        // once someone else pushes on top of it. A branch origin does not
        // have contains nothing; `merge-base` would call that a fatal
        // error rather than a "no".
        let contained = cached
            && self.git_probe(
                id,
                crew,
                &["-C", &cache, "rev-parse", "--verify", "--quiet", &remote],
            )?
            && self.git_probe(
                id,
                crew,
                &[
                    "-C",
                    &cache,
                    "merge-base",
                    "--is-ancestor",
                    &refname,
                    &remote,
                ],
            )?;
        if cached && !contained {
            self.git(
                id,
                crew,
                &[
                    "-C",
                    &ws,
                    "fetch",
                    "--quiet",
                    "--no-auto-gc",
                    "--update-head-ok",
                    &cache,
                    &format!("+{refname}:{refname}"),
                ],
            )?;
            self.git(id, crew, &["-C", &ws, "checkout", "--quiet", branch])?;
            if self.git_probe(
                id,
                crew,
                &["-C", &ws, "rev-parse", "--verify", "--quiet", &remote],
            )? {
                self.git(
                    id,
                    crew,
                    &[
                        "-C",
                        &ws,
                        "branch",
                        "--quiet",
                        &format!("--set-upstream-to=origin/{branch}"),
                        branch,
                    ],
                )?;
            }
        } else {
            // a cache copy origin already contains: origin's is newer
            let start = if contained { branch } else { start_ref };
            self.git(
                id,
                crew,
                &[
                    "-C",
                    &ws,
                    "checkout",
                    "--quiet",
                    "-B",
                    branch,
                    &format!("origin/{start}"),
                ],
            )?;
        }
        Ok(())
    }

    /// Spec N §5: the clone's `refs/heads/<branch>` into the cache, by a
    /// fetch run *in the cache*. A push run in the clone would honour the
    /// clone's config (an `url.<x>.insteadOf` there could aim it at
    /// another crew's cache); `upload-pack` runs in the clone under the
    /// git profile (`sandbox_args`), so it serves only what the agent
    /// could read, and the only write is into the daemon-owned cache. The
    /// `+` is intended: the clone was seeded from the cache's copy, so the
    /// clone's is the newer state even after a rebase. A branch the clone
    /// does not have (deleted by the agent) is skipped.
    ///
    /// The cache's HEAD is detached (`ensure_repo`), so a harvest of the
    /// default branch is a fetch like any other: git refuses to fetch into
    /// the branch HEAD names, and there is none.
    ///
    /// The remote is `workspace/.git` as a `file://` URL (`file_url`),
    /// served by `upload-pack --strict`. Not a plain path: git reads a
    /// local path that is a bundle file (say, `.git` swapped for a symlink
    /// to one) with its bundle transport, itself, as the daemon, and
    /// ignores `--upload-pack`. Without `--strict`, upload-pack tries
    /// `<path>/.git` before `<path>`, and falls back to `workspace/` itself
    /// when `.git` is not a repository — either way a repository the agent
    /// assembled rather than the one `check_clone` inspected.
    /// `before_fetch` runs just before the fetch (a no-op but in `testing`).
    fn harvest(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        branch: &str,
        before_fetch: impl FnOnce(),
    ) -> Result<bool, MaterializeError> {
        let refname = format!("refs/heads/{branch}");
        let present = self.agent_git(
            id,
            crew,
            agent,
            &["rev-parse", "--verify", "--quiet", &refname],
            &[0, 1],
        )?;
        if present.trim().is_empty() {
            return Ok(false);
        }
        // Run by a shell on the fetch's serving side: every word quoted.
        // `env -i` for the same reason `agent_git` clears its environment.
        // `home` lives until the fetch below has returned.
        let home = ScratchHome::new(agent).map_err(|e| MaterializeError::Io {
            id: id.to_string(),
            path: agent.nono_home.clone(),
            message: format!("cannot create nono's home beside it: {e}"),
        })?;
        let mut upload_pack = format!(
            "--upload-pack=env -i HOME={} PATH={} {}",
            sh_quote(&home.path().display().to_string()),
            sh_quote(&outer_path(self.tools)),
            sh_quote(&self.tools.nono.display().to_string()),
        );
        let words = sandbox_args(agent, self.socket_policy, &self.tools.balerix)
            .into_iter()
            .chain([self.tools.git.display().to_string()]);
        for word in words {
            upload_pack.push(' ');
            upload_pack.push_str(&sh_quote(&word));
        }
        upload_pack.push_str(" upload-pack --strict");
        before_fetch();
        let fetched = self
            .git(
                id,
                crew,
                &[
                    "-C",
                    &crew.repo.display().to_string(),
                    "fetch",
                    "--quiet",
                    "--no-auto-gc",
                    &upload_pack,
                    &file_url(&agent.workspace.join(".git")),
                    &format!("+{refname}:{refname}"),
                ],
            )
            .map(|_| true);
        drop(home);
        fetched
    }

    fn record_branch(id: &str, agent: &AgentPaths, branch: &str) -> Result<(), MaterializeError> {
        let marker = agent.branch_marker();
        write_atomic(&marker, branch.as_bytes(), 0o644).map_err(|e| MaterializeError::Io {
            id: id.to_string(),
            path: marker,
            message: e.to_string(),
        })
    }

    /// Deletes the agent's clone after harvesting its assigned branch into
    /// the cache (Spec N §5): the branch the marker recorded — the one
    /// balerix created the clone on — or, with no marker, the branch HEAD
    /// is on. Only that branch survives: other local branches the agent
    /// created, and every file in the tree, ignored files included, go
    /// with the clone (#62). A detached HEAD with no marker, a 0.1.x
    /// worktree (`.git` a file), a bare directory or a missing cache has
    /// nothing to harvest and is deleted as it is. A failed harvest fails
    /// the removal, so the operator sees it rather than losing work;
    /// `--purge` is the way past a clone too broken to read (`remove_crew`
    /// harvests only when the cache stays), and past one `check_clone`
    /// refuses: a purge deletes it without a harvest or a check.
    pub fn harvest_and_remove(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
    ) -> Result<(), MaterializeError> {
        self.harvest_and_remove_after(id, crew, agent, || {}, || {})
    }

    /// `harvest_and_remove`, calling `after_checks` once both checks have
    /// passed and before any git reads the clone's objects, and
    /// `before_fetch` after the harvest's last sandboxed probe and before
    /// its fetch. Production passes no-ops. `testing` passes a rewrite of
    /// the clone to one of them: #70's race, a process the agent detached
    /// past the tmux kill changing the clone under a verdict already
    /// given. The checks always run; nothing here skips them.
    pub(crate) fn harvest_and_remove_after(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        after_checks: impl FnOnce(),
        before_fetch: impl FnOnce(),
    ) -> Result<(), MaterializeError> {
        self.harvest_checked(id, crew, agent, after_checks, before_fetch)?;
        remove_tree(id, &agent.workspace)
    }

    /// The harvest Job's step (Spec O §8.4): `harvest_and_remove` without
    /// the removal, since the Job's claim is mounted read-only and the
    /// operator deletes it afterwards. Returns the branch now in the
    /// cache; `None` when there was nothing to harvest (no clone, no
    /// cache, a detached HEAD, a branch the agent deleted).
    pub fn harvest_only(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
    ) -> Result<Option<String>, MaterializeError> {
        self.harvest_checked(id, crew, agent, || {}, || {})
    }

    /// The checks, the sandbox canary, the config check and the fetch, in
    /// the order `harvest_and_remove_after` documents.
    fn harvest_checked(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        after_checks: impl FnOnce(),
        before_fetch: impl FnOnce(),
    ) -> Result<Option<String>, MaterializeError> {
        if !(crew.repo.join(".git").is_dir() && agent.workspace.join(".git").is_dir()) {
            return Ok(None);
        }
        check_clone(id, crew, agent)?;
        self.prepare_sandbox(id, crew, agent)?;
        self.check_clone_config(id, crew, agent)?;
        after_checks();
        let Some(branch) = self.assigned_branch(id, crew, agent)? else {
            return Ok(None);
        };
        Ok(self
            .harvest(id, crew, agent, &branch, before_fetch)?
            .then_some(branch))
    }

    /// The marker's branch, else HEAD's; `None` for a detached HEAD. A
    /// marker that is absent or empty falls back to HEAD; one that cannot
    /// be read is an error, since guessing would harvest the wrong
    /// branch or none (#74).
    fn assigned_branch(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
    ) -> Result<Option<String>, MaterializeError> {
        let marker = agent.branch_marker();
        match std::fs::read_to_string(&marker) {
            Ok(m) if !m.trim().is_empty() => Ok(Some(m.trim().to_string())),
            Ok(_) => self.head_branch(id, crew, agent),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.head_branch(id, crew, agent),
            Err(e) => Err(MaterializeError::Io {
                id: id.to_string(),
                path: marker,
                message: e.to_string(),
            }),
        }
    }

    /// The branch HEAD is on, `None` when detached. `symbolic-ref -q`
    /// exits 1 for a detached HEAD, silently; anything else git cannot do
    /// with HEAD is an error, not a detached HEAD (#74).
    fn head_branch(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
    ) -> Result<Option<String>, MaterializeError> {
        let head = self.agent_git(
            id,
            crew,
            agent,
            &["symbolic-ref", "-q", "--short", "HEAD"],
            &[0, 1],
        )?;
        let head = head.trim();
        Ok((!head.is_empty()).then(|| head.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use crate::layout::StateLayout;
    use crate::socket_policy::SocketPolicy;
    use crate::tools::{Cmd, CmdOutput};
    use crate::workspace::{
        CloneDecision, ScratchHome, decide_clone, file_url, first_symlink_with, is_gits_answer,
        sandbox_args, sandbox_args_writing, scrub_git_env, sweep_stale_homes, wait_past_second,
    };
    use std::path::Path;
    use std::time::{Duration, UNIX_EPOCH};

    /// #253: the wait ends 20 ms into the second after the index's, and
    /// is never more than 1.1 s.
    #[test]
    fn the_index_refresh_waits_just_past_the_checkouts_second() {
        let at = |ms: u64| UNIX_EPOCH + Duration::from_millis(ms);
        // written at 100.300 s, now 100.400 s: until 101.020 s
        assert_eq!(
            wait_past_second(at(100_300), at(100_400)),
            Duration::from_millis(620)
        );
        assert_eq!(
            wait_past_second(at(100_000), at(100_000)),
            Duration::from_millis(1020)
        );
        // already past: no wait
        assert_eq!(wait_past_second(at(100_300), at(101_020)), Duration::ZERO);
        assert_eq!(wait_past_second(at(100_300), at(105_000)), Duration::ZERO);
        // an mtime ahead of the clock waits no more than the cap
        assert_eq!(
            wait_past_second(at(200_000), at(100_000)),
            Duration::from_millis(1100)
        );
    }

    /// The write grant goes before nono's `--`, after the profile.
    #[test]
    fn the_write_grant_precedes_the_separator() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let agent = layout.agent(&"f/c/a".parse().unwrap());
        let plain = sandbox_args(&agent, SocketPolicy::Open, Path::new("/b"));
        assert!(!plain.iter().any(|a| a == "--allow"));
        let args = sandbox_args_writing(
            &agent,
            SocketPolicy::Open,
            Path::new("/b"),
            Some(Path::new("/w/.git")),
        );
        let n = args.len();
        assert_eq!(args[n - 3..], ["--allow", "/w/.git", "--"]);
        assert_eq!(args[..n - 3], plain[..plain.len() - 1]);
    }

    #[test]
    fn deny_puts_sandbox_exec_after_nonos_separator() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let agent = layout.agent(&"f/c/a".parse().unwrap());
        let args = sandbox_args(&agent, SocketPolicy::Deny, Path::new("/b"));
        assert_eq!(args[args.len() - 4..], ["--", "/b", "sandbox-exec", "--"]);
        for policy in [SocketPolicy::Mediate, SocketPolicy::Open] {
            let args = sandbox_args(&agent, policy, Path::new("/b"));
            assert_eq!(args.last().unwrap(), "--");
            assert!(!args.iter().any(|a| a == "sandbox-exec"), "{policy:?}");
        }
    }

    /// #136: git's own maintenance removes `objects/maintenance.lock`
    /// between the listing and the read. An entry gone by then is skipped;
    /// a symlink beside it is still found, and a missing root still fails.
    #[test]
    fn an_entry_that_vanishes_mid_scan_is_skipped() {
        let root = std::env::temp_dir().join(format!("balerix-vanish-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("pack")).unwrap();
        std::fs::write(root.join("maintenance.lock"), b"").unwrap();
        std::fs::create_dir(root.join("gone-dir")).unwrap();
        let racing_maintenance = |p: &std::path::Path| {
            if p.ends_with("maintenance.lock") {
                let _ = std::fs::remove_file(p);
            }
            if p.ends_with("gone-dir") {
                let _ = std::fs::remove_dir(p);
            }
            std::fs::symlink_metadata(p)
        };
        assert_eq!(first_symlink_with(&root, racing_maintenance).unwrap(), None);

        std::os::unix::fs::symlink("/etc", root.join("pack").join("x")).unwrap();
        std::fs::write(root.join("maintenance.lock"), b"").unwrap();
        assert_eq!(
            first_symlink_with(&root, racing_maintenance).unwrap(),
            Some(root.join("pack").join("x"))
        );

        let missing = root.join("absent");
        let err = first_symlink_with(&missing, racing_maintenance).unwrap_err();
        assert_eq!(err.0, missing);
        assert_eq!(err.1.kind(), std::io::ErrorKind::NotFound);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// #108 review: a SIGKILLed daemon's temp homes are swept; this
    /// process's own, and anything else, are left.
    #[test]
    fn stale_scratch_homes_of_another_pid_are_swept() {
        let dir = tempfile::tempdir().unwrap();
        let layout = crate::StateLayout::from_env(dir.path(), |_| None);
        let agent = layout.agent(&"f/c/a".parse().unwrap());
        let root = agent.nono_home.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&root).unwrap();
        let other = root.join(".nono-git-4194304999-0-1");
        std::fs::create_dir_all(other.join(".local/state/nono/sessions")).unwrap();
        let ours = ScratchHome::new(&agent).unwrap();
        let unrelated = root.join(".nono-gitx");
        std::fs::create_dir_all(&unrelated).unwrap();
        let file = root.join(".nono-git-1-0-0");
        std::fs::write(&file, "").unwrap();

        sweep_stale_homes(&agent);
        assert!(!other.exists(), "another pid's home is swept");
        assert!(ours.path().exists(), "this process's own is in use");
        assert!(unrelated.exists() && file.exists(), "only our directories");
        let path = ours.path().to_path_buf();
        drop(ours);
        assert!(!path.exists());
    }

    /// Spec N amendment §12.2 (#109): git's yes/no probes are silent on
    /// exit 1; nono's own failure also exits 1 and prints `nono: …`.
    #[test]
    fn an_accepted_exit_is_gits_answer_only_when_silent() {
        let out = |code: i32, stderr: &str| CmdOutput {
            stdout: String::new(),
            stderr: stderr.to_string(),
            code,
        };
        assert!(is_gits_answer(&out(0, "")));
        assert!(
            is_gits_answer(&out(0, "warning: something\n")),
            "exit 0 is never inspected"
        );
        assert!(is_gits_answer(&out(1, "")));
        assert!(is_gits_answer(&out(1, " \n")), "blank is silent");
        assert!(!is_gits_answer(&out(
            1,
            "nono: Profile read error at /x: profile file not found\n"
        )));
        assert!(
            !is_gits_answer(&out(1, "warning: something\n")),
            "a git warning beside a no fails closed too"
        );
    }

    fn clean() -> Result<bool, ()> {
        Ok(false)
    }
    fn never() -> Result<bool, ()> {
        panic!("the tree is not inspected on this path")
    }

    /// #67: the variable is set on every git call balerix makes, over
    /// whatever the daemon inherited (`GIT_NO_LAZY_FETCH=0` in its
    /// environment would otherwise reach the harvest's `upload-pack`).
    #[test]
    fn every_git_call_sets_git_no_lazy_fetch() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let probe = dir.path().join("probe.sh");
        std::fs::write(&probe, "#!/bin/sh\nprintf '%s' \"$GIT_NO_LAZY_FETCH\"\n").unwrap();
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let inherited = || Cmd::new(&probe).env("GIT_NO_LAZY_FETCH", "0");
        let sees = |cmd: Cmd| cmd.run().unwrap().stdout;

        assert_eq!(sees(scrub_git_env(inherited())), "1");
        assert_eq!(sees(scrub_git_env(Cmd::new(&probe))), "1");
    }

    /// #70: the harvest names the clone by URL; git percent-decodes it,
    /// so every byte that is not plainly safe is encoded, `%` included.
    #[test]
    fn file_url_encodes_every_byte_git_would_decode_or_misread() {
        use std::os::unix::ffi::OsStrExt;

        assert_eq!(
            file_url(std::path::Path::new("/a/b-c_d.e~f/.git")),
            "file:///a/b-c_d.e~f/.git"
        );
        assert_eq!(
            file_url(std::path::Path::new("/it's a 100%20 root/#x?y")),
            "file:///it%27s%20a%20100%2520%20root/%23x%3Fy"
        );
        assert_eq!(
            file_url(std::path::Path::new("/r\u{e9}pertoire")),
            "file:///r%C3%A9pertoire"
        );
        assert_eq!(
            file_url(std::path::Path::new(std::ffi::OsStr::from_bytes(b"/\xff"))),
            "file:///%FF",
            "a path that is not UTF-8 is encoded byte by byte"
        );
    }

    /// Spec L §12's marker rule over the clone (Spec N §4 step 1).
    #[test]
    fn a_matching_marker_reuses_whatever_head_is() {
        assert_eq!(
            decide_clone(Some("b"), Some("my-fix"), "b", never),
            Ok(CloneDecision::Reuse)
        );
        assert_eq!(
            decide_clone(Some("b\n"), None, "b", never),
            Ok(CloneDecision::Reuse),
            "the marker file ends in whatever write_atomic wrote; trimmed"
        );
    }

    #[test]
    fn a_detached_head_is_reused_as_is() {
        assert_eq!(
            decide_clone(Some("old"), None, "b", never),
            Ok(CloneDecision::Reuse)
        );
        assert_eq!(
            decide_clone(None, None, "b", never),
            Ok(CloneDecision::Reuse)
        );
    }

    #[test]
    fn a_head_already_on_the_branch_is_recorded_without_a_move() {
        assert_eq!(
            decide_clone(Some("old"), Some("b"), "b", never),
            Ok(CloneDecision::Record)
        );
        assert_eq!(
            decide_clone(None, Some("b"), "b", never),
            Ok(CloneDecision::Record),
            "no marker: judged by HEAD once"
        );
    }

    #[test]
    fn another_branch_moves_a_clean_clone_and_fails_a_dirty_one() {
        assert_eq!(
            decide_clone(Some("old"), Some("old"), "b", clean),
            Ok(CloneDecision::Recreate { old: "old".into() })
        );
        assert_eq!(
            decide_clone(Some("old"), Some("my-fix"), "b", || Ok::<_, ()>(true)),
            Ok(CloneDecision::Dirty { old: "old".into() }),
            "the marker names the old branch, not the one the agent is on"
        );
        assert_eq!(
            decide_clone(None, Some("my-fix"), "b", clean),
            Ok(CloneDecision::Recreate {
                old: "my-fix".into()
            }),
            "no marker: HEAD is the old branch"
        );
    }

    #[test]
    fn a_failed_dirty_check_propagates() {
        assert_eq!(
            decide_clone(Some("old"), Some("old"), "b", || Err("status failed")),
            Err("status failed")
        );
    }
}
