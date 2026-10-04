//! What the operator's Jobs do on the shared volume (Spec O §8.3, §8.4,
//! §20.3). Each sees the crew's slice where the agent pod mounts it, so
//! every path a clone or a pool records is the one the agent later reads.
//! `balerix-agent` wraps these as `crew-sync`, `pool-sync` and `harvest`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use balerix_core::{AgentId, CrewRef, FleetName, MaterializeError, RepoRef};

use crate::layout::{PodMounts, SharedSlice, StateLayout};
use crate::toolchain::{Toolchain, drop_stale_marker};
use crate::tools::ToolPaths;
use crate::workspace::Workspace;

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
                &fleet.root.join("logs").join("mise.pools.log"),
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
    Workspace {
        tools,
        gh_config_dir: None,
        cache_is_read_only: false,
    }
    .harvest_only(&id.to_string(), &slice.crew(), &agent)
}
