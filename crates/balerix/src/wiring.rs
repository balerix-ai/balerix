//! The only place adapters meet the process environment.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use balerix_api::{CredentialBundle, FleetSpec, Timestamp};
use balerix_config::{HostDefaults, HostPaths, ResolveOptions, from_value, host, resolve};
use balerix_core::{Clock, CredentialSource, Fleet, FleetName, FleetResolver};
use balerix_runtime::{StateLayout, ToolPaths};
use balerix_server::ServerPaths;
use serde_json::Value;

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

/// The two Spec L ports over `balerix-config`: what
/// `commands::fleet::load_request` does for `up`, with the host's
/// defaults read at call time, so a daemon that outlives a `gh auth
/// login` hands the next managed fleet the new token.
pub struct HostResolver;

/// The pure half: `file` resolved as fleet `name` beneath `defaults`.
pub fn resolve_file(
    file: &Value,
    name: &FleetName,
    defaults: &HostDefaults,
) -> Result<FleetSpec, String> {
    let file = from_value(file).map_err(|e| e.to_string())?;
    let spec = resolve(
        &file,
        &ResolveOptions {
            name_override: Some(name.to_string()),
            host_claude_settings: defaults.claude_settings.clone(),
        },
    )
    .map_err(|e| e.to_string())?;
    // names and repos, as `load_request` checks before any request
    Fleet::try_from(spec.clone()).map_err(|e| e.to_string())?;
    Ok(spec)
}

fn host_defaults() -> Result<HostDefaults, String> {
    let paths = HostPaths::discover().map_err(|e| e.to_string())?;
    host::load(&paths).map_err(|e| e.to_string())
}

impl FleetResolver for HostResolver {
    /// Reads the host's `settings.json` only: a malformed credential file
    /// is `CredentialSource::load`'s 500, never a 400 blamed on the file.
    fn resolve(&self, file: &Value, name: &FleetName) -> Result<FleetSpec, String> {
        let paths = HostPaths::discover().map_err(|e| e.to_string())?;
        let defaults = HostDefaults {
            claude_settings: host::load_settings(&paths).map_err(|e| e.to_string())?,
            ..HostDefaults::default()
        };
        resolve_file(file, name, &defaults)
    }
}

impl CredentialSource for HostResolver {
    fn load(&self) -> Result<CredentialBundle, String> {
        host_defaults().map(|d| d.credentials)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file() -> Value {
        json!({
            "apiVersion": "balerix/v1", "kind": "Fleet",
            "crews": { "c": { "repo": "acme/api", "agents": { "a": { "branch": "feature/x" } } } }
        })
    }

    #[test]
    fn resolve_file_folds_the_host_settings_in_and_keeps_the_branch() {
        let name: FleetName = "f".parse().unwrap();
        let defaults = HostDefaults {
            claude_settings: Some(json!({ "model": "haiku" })),
            ..HostDefaults::default()
        };
        let spec = resolve_file(&file(), &name, &defaults).unwrap();
        assert_eq!(spec.name, "f");
        let a = &spec.crews["c"].agents["a"];
        assert_eq!(a.branch.as_deref(), Some("feature/x"));
        assert_eq!(a.claude.settings["model"], "haiku");
    }

    #[test]
    fn resolve_file_errors_carry_the_config_path() {
        let name: FleetName = "f".parse().unwrap();
        let mut f = file();
        f["crews"]["c"]["agents"]["a"]["tools"] = json!({ "node": "22" });
        let e = resolve_file(&f, &name, &HostDefaults::default()).unwrap_err();
        assert!(e.starts_with("crews.c.agents.a.tools.node:"), "{e}");
        let e =
            resolve_file(&json!({ "kind": "Fleet" }), &name, &HostDefaults::default()).unwrap_err();
        assert!(e.starts_with("file: missing field `apiVersion`"), "{e}");
    }
}
