//! The Jobs' commands (Spec O §8.3, §8.4, §20.3): thin over
//! `balerix_runtime::jobs`, each returning its one-line outcome.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow};
use balerix_core::{AgentId, CrewRef, FleetName, RepoRef};
use balerix_runtime::jobs::{self, PoolLevel};
use balerix_runtime::layout::SharedSlice;
use balerix_runtime::{ToolPaths, embedded_system_tools};

use crate::cli::{CrewSyncArgs, HarvestArgs, Level, PoolSyncArgs};

/// The tools on PATH. The `balerix` slot is this binary: a Job renders no
/// `launch.sh` and runs no relay, the slot only has to name a file.
fn tools() -> Result<ToolPaths> {
    let exe = std::env::current_exe().context("cannot name this binary")?;
    ToolPaths::discover_in(&std::env::var_os("PATH").unwrap_or_default(), &exe)
        .map_err(|e| anyhow!("{e} (a Job needs git, gh, mise, nono and tmux on PATH)"))
}

pub fn crew_sync(args: &CrewSyncArgs) -> Result<String> {
    let crew: CrewRef = args
        .crew
        .parse()
        .map_err(|e| anyhow!("--crew {}: {e}", args.crew))?;
    let repo = RepoRef::parse(&args.repo).map_err(|e| anyhow!("--repo: {e}"))?;
    let token = match &args.gh_token_file {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("cannot read the token file {}", path.display()))?
                .trim_end_matches('\n')
                .to_string(),
        ),
        None => None,
    };
    let table: BTreeMap<String, String> = args.tools.iter().cloned().collect();
    Ok(jobs::sync_crew(
        &tools()?,
        &SharedSlice::new(&args.dirs.shared_dir),
        &args.dirs.scratch_dir,
        &crew,
        &repo,
        &args.git_ref,
        token.as_deref(),
        &table,
    )?)
}

pub fn pool_sync(args: &PoolSyncArgs) -> Result<String> {
    let given: BTreeMap<String, String> = args.tools.iter().cloned().collect();
    let (level, table) = match args.level {
        Level::Daemon if given.is_empty() && !args.no_tools => {
            (PoolLevel::Daemon, embedded_system_tools())
        }
        Level::Daemon => (PoolLevel::Daemon, given),
        Level::Fleet => {
            let name = args
                .fleet
                .as_deref()
                .context("pool-sync --level fleet needs --fleet <name>")?;
            let fleet: FleetName = name.parse().map_err(|e| anyhow!("--fleet {name}: {e}"))?;
            (PoolLevel::Fleet(fleet), given)
        }
    };
    jobs::sync_pool(
        &tools()?,
        &SharedSlice::new(&args.dirs.shared_dir),
        &args.dirs.scratch_dir,
        &level,
        &table,
    )
    .map_err(|e| anyhow!("tools: {e}"))?;
    Ok("synced".to_string())
}

pub fn harvest(args: &HarvestArgs) -> Result<String> {
    let id: AgentId = args
        .agent
        .parse()
        .map_err(|e| anyhow!("--agent {}: {e}", args.agent))?;
    let branch = jobs::harvest(
        &tools()?,
        &SharedSlice::new(&args.dirs.shared_dir),
        &args.dirs.scratch_dir,
        &id,
        &args.agent_dir,
    )?;
    Ok(match branch {
        Some(b) => format!("harvested {b}"),
        None => "nothing to harvest".to_string(),
    })
}
