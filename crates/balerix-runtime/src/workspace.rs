//! Crew object cache and per-agent private clone (Spec N; Phase 2 spec
//! §4.2 step 1 before it).

use std::path::{Path, PathBuf};

use balerix_core::{MaterializeError, RepoRef};

use crate::fsutil::write_atomic;
use crate::home::render_hosts_yml;
use crate::launch::outer_path;
use crate::layout::{AgentPaths, CrewPaths};
use crate::quote::sh_quote;
use crate::sandbox::write_git_profile;
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
/// guards is described at `harden_agent_git`.
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

/// Config and environment for a git call in a repository the agent can
/// write to (its private clone), for the workspace reader (`inspect.rs`);
/// the clone step runs under the git profile instead, whose `set_vars`
/// carry the same variables (`sandbox::render_git_profile`). Command-line
/// config beats every config file, so nothing the agent wrote into its
/// `.git/config` runs as the daemon: fsmonitor off, hooks pointed at an
/// empty directory, no optional locks, no prompt, and the `GIT_*` scrub.
/// `GIT_CEILING_DIRECTORIES` is the agent's own root, the parent of
/// `workspace/`: the agent owns the clone and can delete its `.git`, and
/// repository discovery would then walk up and run the command in whatever
/// repository contains the state root. git only honours a ceiling that
/// matches the resolved path, so it is canonical.
/// `GIT_NO_LAZY_FETCH=1` (from `scrub_git_env`): a promisor remote in the
/// clone's config (`extensions.partialClone`, or any `remote.<x>.promisor`)
/// would otherwise make any call that reads a missing object (`status`,
/// `diff`) fetch it from `remote.<x>.url` as the daemon, into the clone's
/// own object store where the next harvest carries it into the cache, and
/// run `remote.<x>.uploadpack` as the daemon on the way. git honours the
/// variable from 2.45.1 (and the patched maintenance releases from
/// 2.39.4), so it is the second layer: both callers refuse such a config
/// by key before any call that reads an object (`check_clone_config` for
/// the clone step, `refuse_filters` for the workspace reader, #67), on
/// any git.
pub(crate) fn harden_agent_git(cmd: Cmd, crew: &CrewPaths, agent_root: &Path) -> Cmd {
    let no_hooks = crew.no_hooks();
    let _ = std::fs::create_dir_all(&no_hooks);
    let ceiling = agent_root
        .canonicalize()
        .unwrap_or_else(|_| agent_root.to_path_buf());
    scrub_git_env(cmd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CEILING_DIRECTORIES", ceiling.display().to_string())
        .args(["-c", "core.fsmonitor=false"])
        .args([
            "-c".to_string(),
            format!("core.hooksPath={}", no_hooks.display()),
        ])
}

/// Config keys that declare a promisor remote, in the form `git config
/// --name-only` prints them (lowercased). `inspect.rs`'s `FILTER_KEYS`
/// carries the same alternation for the workspace reader.
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
/// none of this. The session is stopped before both callers run, but a process the
/// agent detached can outlive the tmux kill and rewrite the clone after
/// this check (#70). That is why the git calls after it run under the
/// git profile (`agent_git`): this check names the problem for the
/// operator, the profile is what holds. Filesystem only: no git call.
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

/// The first symlink at or under `root`, found without following any.
/// A work list, not recursion: the agent decides how deep the tree goes.
fn first_symlink(root: &Path) -> Result<Option<PathBuf>, (PathBuf, std::io::Error)> {
    let mut todo = vec![root.to_path_buf()];
    while let Some(path) = todo.pop() {
        let meta = std::fs::symlink_metadata(&path).map_err(|e| (path.clone(), e))?;
        if meta.file_type().is_symlink() {
            return Ok(Some(path));
        }
        if meta.is_dir() {
            for entry in std::fs::read_dir(&path).map_err(|e| (path.clone(), e))? {
                todo.push(entry.map_err(|e| (path.clone(), e))?.path());
            }
        }
    }
    Ok(None)
}

pub struct Workspace<'a> {
    pub tools: &'a ToolPaths,
    /// `GH_CONFIG_DIR` for the daemon's git calls when `git.auth: gh`.
    pub gh_config_dir: Option<PathBuf>,
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
        let mut cmd =
            scrub_git_env(Cmd::new(&self.tools.git).log(&crew.root.join("logs").join("git.log")));
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
    /// `agent_git`, the workspace reader's `refuse_filters` probe: it
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
    /// a `version` that must exit 0 tells the two apart, so a nono that
    /// validates but cannot run fails the step instead of reading as "no
    /// promisor keys" or "branch absent".
    fn prepare_sandbox(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
    ) -> Result<(), MaterializeError> {
        write_git_profile(self.tools, id, agent, crew)?;
        self.agent_git(id, crew, agent, &["version"], &[0])
            .map(|_| ())
    }

    /// `nono`'s arguments up to and including the git binary, for a git
    /// call in `agent`'s existing clone: `-s --log-file <logs>/nono-git.log
    /// run --profile <git profile> -- <git>`. The profile is
    /// `sandbox::render_git_profile`; `write_git_profile` must have run.
    fn sandbox_args(&self, agent: &AgentPaths) -> Vec<String> {
        vec![
            "-s".into(),
            "--log-file".into(),
            agent.logs.join("nono-git.log").display().to_string(),
            "run".into(),
            "--profile".into(),
            agent.git_profile().display().to_string(),
            "--".into(),
            self.tools.git.display().to_string(),
        ]
    }

    /// One git call inside the agent's existing clone, run under the git
    /// profile (`sandbox::render_git_profile`; Spec N amendment
    /// 2026-10-01, #68, #70): it reads the clone and the crew cache's
    /// objects, writes nothing and has no network, so whatever the clone
    /// points at, git sees no more than the agent could. The checks
    /// before it give the operator a readable refusal; the profile is the
    /// boundary, and holds even when the clone changes after the checks.
    ///
    /// nono starts from an empty environment (`HOME` is the agent's
    /// `nono/`, as for `launch.sh`), and the profile's `set_vars` carry
    /// the hardening `harden_agent_git` sets for the workspace reader;
    /// the `-c` pairs are the same. `--git-dir` names `workspace/.git`
    /// exactly: with `-C` alone, a `.git` git rejects (say, its `HEAD`
    /// deleted) makes git take `workspace/` itself for a bare repository.
    /// `accepted` are the exit codes that count as success (0 included).
    /// A failure is reported as git's, with git's subcommand, not nono's.
    fn agent_git(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        args: &[&str],
        accepted: &[i32],
    ) -> Result<String, MaterializeError> {
        let cmd = Cmd::new(&self.tools.nono)
            .env_clear()
            .env("HOME", agent.nono_home.display().to_string())
            .env("PATH", outer_path(self.tools))
            .log(&crew.root.join("logs").join("git.log"))
            .args(self.sandbox_args(agent))
            .args(["-c", "core.fsmonitor=false"])
            .args([
                "-c".to_string(),
                format!("core.hooksPath={}", crew.no_hooks().display()),
                "-C".to_string(),
                agent.workspace.display().to_string(),
                format!("--git-dir={}", agent.workspace.join(".git").display()),
                format!("--work-tree={}", agent.workspace.display()),
            ])
            .args(args.iter().copied());
        cmd.run_with_exit_codes(accepted)
            .map(|o| o.stdout)
            .map_err(|f| MaterializeError::Tool {
                id: id.to_string(),
                tool: "git".into(),
                subcommand: args.first().copied().unwrap_or_default().to_string(),
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
        let dot_git = agent.workspace.join(".git");
        if dot_git.is_dir() {
            check_clone(id, crew, agent)?;
            let marker = std::fs::read_to_string(agent.branch_marker()).ok();
            if marker.as_deref().map(str::trim) == Some(branch) {
                // Spec L §12: marker equals `branch` → done, with no git
                // call; `decide_clone` agrees (`Reuse`)
                return Ok(());
            }
            self.prepare_sandbox(id, crew, agent)?;
            self.check_clone_config(id, crew, agent)?;
            let head = self.head_branch(id, crew, agent)?;
            let decision = decide_clone(marker.as_deref(), head.as_deref(), branch, || {
                self.agent_git(id, crew, agent, &["status", "--porcelain"], &[0])
                    .map(|s| !s.trim().is_empty())
            })?;
            match decision {
                CloneDecision::Reuse => return Ok(()),
                CloneDecision::Record => return Self::record_branch(id, agent, branch),
                CloneDecision::Dirty { old } => {
                    return Err(MaterializeError::Invalid {
                        id: id.to_string(),
                        message: format!(
                            "the clone was created on branch {old:?} but the agent's \
                             branch is {branch:?}, and the clone has local changes; \
                             commit or discard them in {} first",
                            agent.workspace.display()
                        ),
                    });
                }
                CloneDecision::Recreate { old } => {
                    self.harvest(id, crew, agent, &old)?;
                    remove_tree(id, &agent.workspace)?;
                }
            }
        } else if dot_git.exists() {
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
        } else {
            remove_tree(id, &agent.workspace)?;
        }
        self.create_clone(id, crew, agent, repo, branch, start_ref)
            .map_err(|e| discard_half_made(id, &agent.workspace, e))?;
        Self::record_branch(id, agent, branch)
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
        self.git(
            id,
            crew,
            &["-C", &cache, "fetch", "--quiet", "--no-auto-gc", "origin"],
        )?;
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
        let cached = self.git_probe(
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
    /// git profile (`agent_git`), so it serves only what the agent could
    /// read, and the only write is into the daemon-owned cache. The `+` is intended: the clone was seeded from
    /// the cache's copy, so the clone's is the newer state even after a
    /// rebase. A branch the clone does not have (deleted by the agent) is
    /// skipped.
    ///
    /// The cache's HEAD is detached (`ensure_repo`), so a harvest of the
    /// default branch is a fetch like any other: git refuses to fetch into
    /// the branch HEAD names, and there is none.
    ///
    /// The remote is `workspace/.git`, served by `upload-pack --strict`:
    /// without `--strict`, upload-pack tries `<path>/.git` before `<path>`,
    /// and falls back to `workspace/` itself when `.git` is not a
    /// repository — either way a repository the agent assembled rather
    /// than the one `check_clone` inspected.
    fn harvest(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        branch: &str,
    ) -> Result<(), MaterializeError> {
        let refname = format!("refs/heads/{branch}");
        let present = self.agent_git(
            id,
            crew,
            agent,
            &["rev-parse", "--verify", "--quiet", &refname],
            &[0, 1],
        )?;
        if present.trim().is_empty() {
            return Ok(());
        }
        // Run by a shell on the fetch's serving side: every word quoted.
        // `env -i` for the same reason `agent_git` clears its environment.
        let mut upload_pack = format!(
            "--upload-pack=env -i HOME={} PATH={} {}",
            sh_quote(&agent.nono_home.display().to_string()),
            sh_quote(&outer_path(self.tools)),
            sh_quote(&self.tools.nono.display().to_string()),
        );
        for word in self.sandbox_args(agent) {
            upload_pack.push(' ');
            upload_pack.push_str(&sh_quote(&word));
        }
        upload_pack.push_str(" upload-pack --strict");
        self.git(
            id,
            crew,
            &[
                "-C",
                &crew.repo.display().to_string(),
                "fetch",
                "--quiet",
                "--no-auto-gc",
                &upload_pack,
                &agent.workspace.join(".git").display().to_string(),
                &format!("+{refname}:{refname}"),
            ],
        )
        .map(|_| ())
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
        self.harvest_and_remove_after(id, crew, agent, || {})
    }

    /// `harvest_and_remove`, calling `after_checks` once both checks have
    /// passed and before any git reads the clone's objects. Production
    /// passes a no-op. `testing::harvest_and_remove_racing` passes a
    /// rewrite of the clone: #70's race, a process the agent detached
    /// past the tmux kill changing the clone under a verdict already
    /// given. The checks always run; nothing here skips them.
    pub(crate) fn harvest_and_remove_after(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        after_checks: impl FnOnce(),
    ) -> Result<(), MaterializeError> {
        if crew.repo.join(".git").is_dir() && agent.workspace.join(".git").is_dir() {
            check_clone(id, crew, agent)?;
            self.prepare_sandbox(id, crew, agent)?;
            self.check_clone_config(id, crew, agent)?;
            after_checks();
            if let Some(branch) = self.assigned_branch(id, crew, agent)? {
                self.harvest(id, crew, agent, &branch)?;
            }
        }
        remove_tree(id, &agent.workspace)
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
    use crate::tools::Cmd;
    use crate::workspace::{CloneDecision, decide_clone, harden_agent_git, scrub_git_env};

    fn clean() -> Result<bool, ()> {
        Ok(false)
    }
    fn never() -> Result<bool, ()> {
        panic!("the tree is not inspected on this path")
    }

    /// #67: the variable is set on every git call balerix makes, over
    /// whatever the daemon inherited (`GIT_NO_LAZY_FETCH=0` in its
    /// environment would otherwise reach the harvest's `upload-pack`).
    /// The probe is a script that ignores its arguments, since the
    /// hardened builder adds `-c` pairs a shell would read as a command.
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

        let layout = crate::StateLayout::from_env(dir.path(), |_| None);
        let crew = layout.crew(&"f/c".parse().unwrap());
        assert_eq!(sees(harden_agent_git(inherited(), &crew, dir.path())), "1");
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
