//! Crew object cache and per-agent private clone (Spec N; Phase 2 spec
//! §4.2 step 1 before it).

use std::path::{Path, PathBuf};

use balerix_core::{MaterializeError, RepoRef};

use crate::fsutil::write_atomic;
use crate::home::render_hosts_yml;
use crate::layout::{AgentPaths, CrewPaths};
use crate::tools::{Cmd, ToolPaths};

/// `git` honours `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_PREFIX`
/// and `GIT_COMMON_DIR` from the environment over an explicit `-C`: if any
/// of these leak in (a pre-commit hook exports them, and so does a daemon
/// started under one), every `-C` call would silently operate on whatever
/// repository those variables name. Scrubbed from every git call balerix
/// makes.
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
    cmd
}

/// Config and environment for a git call in a repository the agent can
/// write to (its private clone), shared by the workspace reader
/// (`inspect.rs`) and the clone step (Spec N §4 step 1, #62). Command-line
/// config beats every config file, so nothing the agent wrote into its
/// `.git/config` runs as the daemon: fsmonitor off, hooks pointed at an
/// empty directory, no optional locks, no prompt, and the `GIT_*` scrub.
/// `GIT_CEILING_DIRECTORIES` is the agent's own root, the parent of
/// `workspace/`: the agent owns the clone and can delete its `.git`, and
/// repository discovery would then walk up and run the command in whatever
/// repository contains the state root. git only honours a ceiling that
/// matches the resolved path, so it is canonical.
pub(crate) fn harden_agent_git(cmd: Cmd, crew: &CrewPaths, agent_root: &Path) -> Cmd {
    let no_hooks = crew.root.join("no-hooks");
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
        cmd.run()
            .map(|o| o.stdout)
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
    /// never loses one. A cache that exists is left alone, so a
    /// steady-state pass costs no git call (Phase 3 spec §6.1);
    /// `ensure_clone` fetches when it actually needs `origin/<ref>`.
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
        self.git(id, crew, &["-C", &cache, "config", "gc.auto", "0"])?;
        Ok(())
    }

    /// One git call inside the agent's clone, hardened by
    /// `harden_agent_git` because the clone is agent-writable. `accepted`
    /// are the exit codes that count as success (0 included).
    fn agent_git(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        args: &[&str],
        accepted: &[i32],
    ) -> Result<String, MaterializeError> {
        let cmd = harden_agent_git(
            Cmd::new(&self.tools.git).log(&crew.root.join("logs").join("git.log")),
            crew,
            &agent.root,
        )
        .args(["-C".to_string(), agent.workspace.display().to_string()])
        .args(args.iter().copied());
        cmd.run_with_exit_codes(accepted)
            .map(|o| o.stdout)
            .map_err(|f| MaterializeError::Tool {
                id: id.to_string(),
                tool: f.tool,
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
    /// branches. Every git call on an existing clone is `agent_git` (#62).
    ///
    /// A `workspace/.git` *file* is a worktree from balerix 0.1, refused
    /// with the remedy (N-6). A `workspace/` with no `.git` at all (a
    /// clone that crashed half-way) is replaced.
    ///
    /// A new clone starts with a fetch in the cache — the one moment
    /// `origin/<start_ref>` must be current, and what makes the clone
    /// cheap — then seeds `branch` from the cache's harvested copy when
    /// there is one, else creates it from `origin/<start_ref>`. A seeded
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
            let marker = std::fs::read_to_string(agent.branch_marker()).ok();
            // Err is a detached HEAD (`ref HEAD is not a symbolic ref`),
            // reused as is: its commits may be on no branch.
            let head = self
                .agent_git(id, crew, agent, &["symbolic-ref", "--short", "HEAD"], &[0])
                .ok();
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
        if let Err(e) = self.create_clone(id, crew, agent, repo, branch, start_ref) {
            let _ = std::fs::remove_dir_all(&agent.workspace);
            return Err(e);
        }
        Self::record_branch(id, agent, branch)
    }

    /// Spec N §4 step 3. These calls run in a clone the agent has never
    /// touched, so they need no hardening; they go through `git` for the
    /// credential helper the clone needs.
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
        let harvested = self
            .git(
                id,
                crew,
                &["-C", &cache, "rev-parse", "--verify", "--quiet", &refname],
            )
            .is_ok();
        if harvested {
            self.git(
                id,
                crew,
                &[
                    "-C",
                    &ws,
                    "fetch",
                    "--quiet",
                    "--no-auto-gc",
                    &cache,
                    &format!("{refname}:{refname}"),
                ],
            )?;
            self.git(id, crew, &["-C", &ws, "checkout", "--quiet", branch])?;
            let remote = format!("refs/remotes/origin/{branch}");
            if self
                .git(
                    id,
                    crew,
                    &["-C", &ws, "rev-parse", "--verify", "--quiet", &remote],
                )
                .is_ok()
            {
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
            self.git(
                id,
                crew,
                &[
                    "-C",
                    &ws,
                    "checkout",
                    "--quiet",
                    "-b",
                    branch,
                    &format!("origin/{start_ref}"),
                ],
            )?;
        }
        Ok(())
    }

    /// Spec N §5: the clone's `refs/heads/<branch>` into the cache, by a
    /// fetch run *in the cache*. A push run in the clone would honour the
    /// clone's config (an `url.<x>.insteadOf` there could aim it at
    /// another crew's cache); `upload-pack` in the clone takes no
    /// repo-local hook or program config, and the only write is into the
    /// daemon-owned cache. The `+` is intended: the clone was seeded from
    /// the cache's copy, so the clone's is the newer state even after a
    /// rebase. A branch the clone does not have (deleted by the agent) is
    /// skipped.
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
        self.git(
            id,
            crew,
            &[
                "-C",
                &crew.repo.display().to_string(),
                "fetch",
                "--quiet",
                "--no-auto-gc",
                &agent.workspace.display().to_string(),
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
    /// harvests only when the cache stays).
    pub fn harvest_and_remove(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
    ) -> Result<(), MaterializeError> {
        if crew.repo.join(".git").is_dir()
            && agent.workspace.join(".git").is_dir()
            && let Some(branch) = self.assigned_branch(id, crew, agent)
        {
            self.harvest(id, crew, agent, &branch)?;
        }
        remove_tree(id, &agent.workspace)
    }

    /// The marker's branch, else HEAD's; `None` for a detached HEAD.
    fn assigned_branch(&self, id: &str, crew: &CrewPaths, agent: &AgentPaths) -> Option<String> {
        match std::fs::read_to_string(agent.branch_marker()) {
            Ok(m) if !m.trim().is_empty() => Some(m.trim().to_string()),
            _ => self
                .agent_git(id, crew, agent, &["symbolic-ref", "--short", "HEAD"], &[0])
                .ok()
                .map(|h| h.trim().to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::workspace::{CloneDecision, decide_clone};

    fn clean() -> Result<bool, ()> {
        Ok(false)
    }
    fn never() -> Result<bool, ()> {
        panic!("the tree is not inspected on this path")
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
