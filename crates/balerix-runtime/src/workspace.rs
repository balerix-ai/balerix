//! Crew clone and per-agent worktree (Phase 2 spec §4.2 step 1, P2-6).

use std::path::{Path, PathBuf};

use balerix_core::{MaterializeError, RepoRef};

use crate::fsutil::write_atomic;
use crate::home::render_hosts_yml;
use crate::layout::CrewPaths;
use crate::tools::{Cmd, ToolPaths};

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

    /// `git` honours `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_PREFIX`
    /// and `GIT_COMMON_DIR` from the environment over an explicit `-C`: if any
    /// of these leak in (e.g. from a pre-commit hook, which git sets for a
    /// linked worktree, or from a daemon started under one), every `-C` call
    /// below would silently operate on whatever repository those variables
    /// name instead of `crew.repo`. Scrub them from every invocation.
    fn git(&self, id: &str, crew: &CrewPaths, args: &[&str]) -> Result<String, MaterializeError> {
        let mut cmd = Cmd::new(&self.tools.git).log(&crew.root.join("logs").join("git.log"));
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_PREFIX",
            "GIT_COMMON_DIR",
        ] {
            cmd = cmd.env_remove(var);
        }
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

    /// Clone without a checkout if absent; otherwise nothing (Phase 3 spec
    /// §6.1: a steady-state pass costs no git call). `ensure_worktree`
    /// fetches when it actually needs `origin/<ref>`.
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
        self.git(
            id,
            crew,
            &[
                "clone",
                "--quiet",
                "--no-checkout",
                &repo.clone_url(),
                &crew.repo.display().to_string(),
            ],
        )?;
        Ok(())
    }

    /// Whether git knows `workspace` as a worktree of `crew.repo`.
    fn is_registered(
        &self,
        id: &str,
        crew: &CrewPaths,
        workspace: &Path,
    ) -> Result<bool, MaterializeError> {
        let repo = crew.repo.display().to_string();
        let list = self.git(id, crew, &["-C", &repo, "worktree", "list", "--porcelain"])?;
        Ok(list
            .lines()
            .any(|l| l.strip_prefix("worktree ").map(Path::new) == Some(workspace)))
    }

    /// Reuses a registered worktree created on `branch`; reuses an existing
    /// branch; otherwise creates the branch from `origin/<git_ref>`.
    ///
    /// `marker` records the branch balerix last created the worktree on.
    /// Spec L §6: when it differs from `branch` (the agent's `branch` was
    /// added, changed or removed) the worktree is re-created on `branch`
    /// if its tree is clean; a dirty tree fails rather than lose the
    /// agent's uncommitted work. The old branch stays in the crew clone.
    /// A matching marker reuses the worktree whatever its HEAD: an agent
    /// that checked out a branch of its own keeps it across restarts
    /// (#60). A worktree without a marker (from before the marker existed)
    /// is judged by its HEAD once, and the marker written then. A detached
    /// HEAD is reused as is: its commits may be on no branch.
    pub fn ensure_worktree(
        &self,
        id: &str,
        crew: &CrewPaths,
        workspace: &Path,
        marker: &Path,
        branch: &str,
        git_ref: &str,
    ) -> Result<(), MaterializeError> {
        let repo = crew.repo.display().to_string();
        let registered = self.is_registered(id, crew, workspace)?;
        if registered && workspace.join(".git").exists() {
            let recorded = std::fs::read_to_string(marker).ok();
            if recorded.as_deref().map(str::trim) == Some(branch) {
                return Ok(());
            }
            let ws = workspace.display().to_string();
            let Ok(head) = self.git(id, crew, &["-C", &ws, "symbolic-ref", "--short", "HEAD"])
            else {
                return Ok(()); // detached
            };
            let head = head.trim();
            if head == branch {
                return Self::record_branch(id, marker, branch);
            }
            let was = recorded.as_deref().map_or(head, str::trim);
            let status = self.git(id, crew, &["-C", &ws, "status", "--porcelain"])?;
            if !status.trim().is_empty() {
                return Err(MaterializeError::Invalid {
                    id: id.to_string(),
                    message: format!(
                        "the worktree was created on branch {was:?} but the agent's \
                         branch is {branch:?}, and the worktree has local changes; \
                         commit or discard them in {ws} first"
                    ),
                });
            }
            self.git(
                id,
                crew,
                &["-C", &repo, "worktree", "remove", "--force", &ws],
            )?;
        }
        self.git(id, crew, &["-C", &repo, "worktree", "prune"])?;
        let ws = workspace.display().to_string();
        let branch_exists = self
            .git(
                id,
                crew,
                &[
                    "-C",
                    &repo,
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ],
            )
            .is_ok();
        if branch_exists {
            self.git(
                id,
                crew,
                &["-C", &repo, "worktree", "add", "--quiet", &ws, branch],
            )?;
        } else {
            // the only moment `origin/<ref>` must be current
            self.git(id, crew, &["-C", &repo, "fetch", "--quiet", "origin"])?;
            self.git(
                id,
                crew,
                &[
                    "-C",
                    &repo,
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    branch,
                    &ws,
                    &format!("origin/{git_ref}"),
                ],
            )?;
        }
        Self::record_branch(id, marker, branch)
    }

    fn record_branch(id: &str, marker: &Path, branch: &str) -> Result<(), MaterializeError> {
        write_atomic(marker, branch.as_bytes(), 0o644).map_err(|e| MaterializeError::Io {
            id: id.to_string(),
            path: marker.to_path_buf(),
            message: e.to_string(),
        })
    }

    pub fn remove_worktree(
        &self,
        id: &str,
        crew: &CrewPaths,
        workspace: &Path,
    ) -> Result<(), MaterializeError> {
        if !crew.repo.join(".git").is_dir() {
            return Ok(());
        }
        let repo = crew.repo.display().to_string();
        // `git worktree remove` errors on a path git does not know as a
        // worktree ("is not a working tree"), which would fail the whole
        // removal over a leftover plain directory. Only ask git to remove
        // what git registered; `prune` and the caller's `rm -rf` clean up
        // anything else.
        if workspace.exists() && self.is_registered(id, crew, workspace)? {
            self.git(
                id,
                crew,
                &[
                    "-C",
                    &repo,
                    "worktree",
                    "remove",
                    "--force",
                    &workspace.display().to_string(),
                ],
            )?;
        }
        self.git(id, crew, &["-C", &repo, "worktree", "prune"])
            .map(|_| ())
    }
}
