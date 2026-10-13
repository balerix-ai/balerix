//! What the operator's Jobs do on the shared volume (Spec O §8.3, §8.4,
//! §20.3). Each sees the crew's slice where the agent pod mounts it, so
//! every path a clone or a pool records is the one the agent later reads.
//! `balerix-agent` wraps these as `crew-sync`, `pool-sync` and `harvest`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use balerix_core::{AgentId, CrewRef, FleetName, MaterializeError, RepoRef};

use crate::layout::{PodMounts, SharedSlice, StateLayout};
use crate::socket_policy::SocketPolicy;
use crate::toolchain::{Toolchain, drop_stale_marker};
use crate::tools::ToolPaths;
use crate::workspace::{Settled, Workspace, dirty_clone};

/// Which half of a crew sync failed: the operator sets `CacheReady` or
/// `ToolsReady` from the prefix (§5.3).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SyncError {
    #[error("cache: {0}")]
    Cache(MaterializeError),
    #[error("tools: {0}")]
    Tools(MaterializeError),
}

/// The two pools above a crew's. Each has a Job of its own, so two crews
/// of one fleet never install the fleet pool at once (§20.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolLevel {
    Daemon,
    Fleet(FleetName),
}

/// The crew cache, made or fetched, then the crew pool, with the fleet
/// and daemon pools as read-only parents. `gh_token` is written under
/// `scratch` for git's credential helper, never onto the volume. Returns
/// the commit `origin/<git_ref>` is at.
#[allow(clippy::too_many_arguments)]
pub fn sync_crew(
    tools: &ToolPaths,
    slice: &SharedSlice,
    scratch: &Path,
    crew: &CrewRef,
    repo: &RepoRef,
    git_ref: &str,
    gh_token: Option<&str>,
    table: &BTreeMap<String, String>,
) -> Result<String, SyncError> {
    let id = crew.to_string();
    let paths = slice.crew();
    let gh_config_dir = match gh_token {
        Some(token) => {
            let dir = scratch.join("gh");
            Workspace::write_fleet_gh_config(&dir, token, &id).map_err(SyncError::Cache)?;
            Some(dir)
        }
        None => None,
    };
    let commit = Workspace {
        tools,
        gh_config_dir,
        cache_is_read_only: false,
        git_read: &[],
        socket_policy: SocketPolicy::Open,
    }
    .sync_cache(&id, &paths, repo, git_ref)
    .map_err(SyncError::Cache)?;
    install(
        tools,
        scratch,
        &id,
        &format!("crew {crew}"),
        &paths.mise_toml(),
        &paths.mise_pool(),
        &[slice.fleet().mise_pool(), slice.daemon_pool()],
        &paths.installed_marker(),
        table,
        &paths.logs.join("mise.pools.log"),
    )
    .map_err(SyncError::Tools)?;
    Ok(commit)
}

/// One of the upper pools, installed from `table`.
pub fn sync_pool(
    tools: &ToolPaths,
    slice: &SharedSlice,
    scratch: &Path,
    level: &PoolLevel,
    table: &BTreeMap<String, String>,
) -> Result<(), MaterializeError> {
    match level {
        PoolLevel::Daemon => {
            let root = slice.daemon_root();
            install(
                tools,
                scratch,
                "system",
                "system",
                &root.join("mise.toml"),
                &slice.daemon_pool(),
                &[],
                &root.join("mise.installed"),
                table,
                &root.join("logs").join("mise.system.log"),
            )
        }
        PoolLevel::Fleet(name) => {
            let fleet = slice.fleet();
            install(
                tools,
                scratch,
                name.as_str(),
                &format!("fleet {name}"),
                &fleet.mise_toml,
                &fleet.mise_pool(),
                &[slice.daemon_pool()],
                &fleet.installed_marker(),
                table,
                &fleet.logs().join("mise.pools.log"),
            )
        }
    }
}

/// One level into its pool. The pool directory is made even for an empty
/// table: the agent pod mounts it as a sub-path, which must exist. A
/// marker that outlived its pool is dropped first, so `install_level`
/// cannot report an empty directory as installed (Spec F, F-5).
#[allow(clippy::too_many_arguments)]
fn install(
    tools: &ToolPaths,
    scratch: &Path,
    id: &str,
    label: &str,
    toml: &Path,
    pool: &Path,
    parents: &[PathBuf],
    marker: &Path,
    table: &BTreeMap<String, String>,
    log: &Path,
) -> Result<(), MaterializeError> {
    drop_stale_marker(id, pool, marker)?;
    std::fs::create_dir_all(pool).map_err(|e| MaterializeError::Io {
        id: id.to_string(),
        path: pool.to_path_buf(),
        message: e.to_string(),
    })?;
    // `install_level` takes every path as an argument; the layout is only
    // what `Toolchain::install`, the agent's own step, reads.
    let layout = StateLayout::xdg(
        scratch.join("state"),
        scratch.join("data"),
        scratch.join("config"),
    );
    Toolchain {
        tools,
        layout: &layout,
    }
    .install_level(id, label, toml, pool, parents, marker, table, log, None)
}

/// The cleanup Job of a Fleet deleted with `retain: None` (Spec O §5.2,
/// §8.5): empties the crew's cache and its pool directory on the shared
/// volume. The two directories are mount points in the Job, so they are
/// emptied, not removed. Nothing the crew only reads (the fleet and
/// daemon pools) is touched.
pub fn remove_crew(slice: &SharedSlice) -> std::io::Result<()> {
    let crew = slice.crew();
    for dir in [&crew.repo, &crew.root] {
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
        }
    }
    Ok(())
}

/// What the branch-change Job did to the clone on the claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchChange {
    /// The clone was on `old`: `old` into the cache when the clone had
    /// it (`harvested`), and the clone removed.
    Removed { old: String, harvested: bool },
    /// The clone stays: its marker or HEAD is the branch, or HEAD is
    /// detached. The pod's `ensure_clone` writes the marker if needed.
    Kept,
    /// No clone; whatever a half-made one left is removed.
    NoClone,
    /// `purge`: the clone removed without a check or a harvest.
    Purged,
}

/// The branch-change Job (Spec O §27): an agent's branch changed while
/// its claim holds a clone. `Workspace::settle_clone`, the host's own
/// judgement, with the crew cache writable: a clean clone on the old
/// branch has that branch harvested into the cache and is removed, so
/// the next pod clones `branch` afresh; a dirty one, or a 0.1
/// worktree, is refused and kept; one already on `branch`, or with a
/// detached HEAD, is kept. With `purge`, the clone is removed unread, as
/// `--purge` removes one.
///
/// Only `workspace/` under `claim` is written, and it is emptied, not
/// removed: in the Job it is the one read-write mount of the claim.
/// The git profile, nono's home and its logs are under `scratch`.
pub fn change_branch(
    tools: &ToolPaths,
    slice: &SharedSlice,
    scratch: &Path,
    id: &AgentId,
    claim: &Path,
    branch: &str,
    purge: bool,
) -> Result<BranchChange, MaterializeError> {
    let agent = job_agent_paths(slice, scratch, id, claim);
    let ws = &agent.workspace;
    if purge {
        empty_workspace(id, ws)?;
        return Ok(BranchChange::Purged);
    }
    let settled = Workspace {
        tools,
        gh_config_dir: None,
        cache_is_read_only: false,
        git_read: &[],
        socket_policy: SocketPolicy::Open,
    }
    .settle_clone(&id.to_string(), &slice.crew(), &agent, branch)?;
    let refuse = |message: String| MaterializeError::Invalid {
        id: id.to_string(),
        message,
    };
    match settled {
        Settled::Kept | Settled::Record => Ok(BranchChange::Kept),
        Settled::NoClone => {
            empty_workspace(id, ws)?;
            Ok(BranchChange::NoClone)
        }
        Settled::Remove { old, harvested } => {
            empty_workspace(id, ws)?;
            Ok(BranchChange::Removed { old, harvested })
        }
        Settled::Dirty { old } => Err(refuse(dirty_clone(&old, branch))),
        Settled::Worktree => Err(refuse(format!(
            "{}: created by balerix 0.1 as a worktree, which cannot be harvested; \
             annotate the Agent balerix.ai/purge=true to remove it (push unpushed work first)",
            ws.display()
        ))),
    }
}

/// Empties `workspace/` (a mount point in the Job, so never removed).
/// `.git` is renamed aside first, so a Job stopped part-way leaves no
/// half-deleted clone to be judged: what is left has no `.git`, and the
/// next run, or the pod's `ensure_clone`, removes it. Entries are
/// removed by their own type: `remove_dir_all` and `remove_file` remove
/// a symlink, never what it points at.
fn empty_workspace(id: &AgentId, ws: &Path) -> Result<(), MaterializeError> {
    let io = |path: &Path, e: std::io::Error| MaterializeError::Io {
        id: id.to_string(),
        path: path.to_path_buf(),
        message: e.to_string(),
    };
    let remove = |path: &Path| -> Result<(), MaterializeError> {
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io(path, e)),
        };
        if meta.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        }
        .map_err(|e| io(path, e))
    };
    let entries = match std::fs::read_dir(ws) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io(ws, e)),
    };
    let aside = ws.join(".git.removing");
    remove(&aside)?;
    let dot_git = ws.join(".git");
    if std::fs::symlink_metadata(&dot_git).is_ok() {
        std::fs::rename(&dot_git, &aside).map_err(|e| io(&dot_git, e))?;
    }
    for entry in entries {
        remove(&entry.map_err(|e| io(ws, e))?.path())?;
    }
    remove(&aside)
}

/// The agent's paths on a claim mounted at `claim`, as the agent pod has
/// them, with the git profile, nono's home and the logs under `scratch`.
fn job_agent_paths(
    slice: &SharedSlice,
    scratch: &Path,
    id: &AgentId,
    claim: &Path,
) -> crate::layout::AgentPaths {
    let layout = StateLayout::pod(
        PodMounts {
            agent: claim.to_path_buf(),
            shared: slice.root.clone(),
            run: scratch.join("run"),
        },
        id,
    );
    let mut agent = layout.agent(id);
    agent.nono_home = scratch.join("nono");
    agent.logs = scratch.join("logs");
    agent.git_profile = scratch.join("nono-git-profile.json");
    agent
}

/// The agent's branch into the crew cache, from a claim mounted read-only
/// at `claim`: the checks and the fetch of `harvest_and_remove`, with the
/// git profile, nono's home and its logs under `scratch`. Removes nothing.
pub fn harvest(
    tools: &ToolPaths,
    slice: &SharedSlice,
    scratch: &Path,
    id: &AgentId,
    claim: &Path,
) -> Result<Option<String>, MaterializeError> {
    let agent = job_agent_paths(slice, scratch, id, claim);
    Workspace {
        tools,
        gh_config_dir: None,
        cache_is_read_only: false,
        git_read: &[],
        socket_policy: SocketPolicy::Open,
    }
    .harvest_only(&id.to_string(), &slice.crew(), &agent)
}
