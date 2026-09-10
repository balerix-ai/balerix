//! The only place adapters meet the process environment.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use balerix_api::Timestamp;
use balerix_core::Clock;
use balerix_runtime::{StateLayout, ToolPaths};
use balerix_server::ServerPaths;

pub fn layout_from_env() -> Result<StateLayout> {
    let home = std::env::home_dir().context("cannot determine the home directory")?;
    Ok(StateLayout::from_env(&home, |k| std::env::var_os(k)))
}

pub fn tool_paths() -> Result<ToolPaths> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let me = std::env::current_exe().context("cannot determine balerix's own path")?;
    ToolPaths::discover_in(&path, &me).map_err(|e| {
        anyhow::anyhow!("{e} (balerix needs git, gh, mise, nono and tmux on PATH; see mise.toml)")
    })
}

/// Wall clock in whole seconds.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        )
    }
}

pub fn server_paths(layout: &StateLayout) -> ServerPaths {
    ServerPaths::new(layout.server_dir())
}
