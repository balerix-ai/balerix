//! Where everything lives (Phase 2 spec §4.1).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use balerix_core::{AgentId, AgentName, CrewRef, FleetName};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateLayout {
    pub state_root: PathBuf,
    pub data_root: PathBuf,
    pub config_root: PathBuf,
}

/// Where one fleet's own files live. `mise_pool()` holds the tools declared
/// in the fleet file's top-level `defaults.tools`, shared read-only with
/// every agent of every crew in the fleet (Spec E §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetPaths {
    pub root: PathBuf,
    pub mise_toml: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrewPaths {
    pub root: PathBuf,
    pub repo: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPaths {
    pub root: PathBuf,
    pub home: PathBuf,
    pub workspace: PathBuf,
    /// nono's own `$HOME` — its state root must not overlap `home/` (P2-5).
    pub nono_home: PathBuf,
    pub mise_toml: PathBuf,
    pub profile: PathBuf,
    pub launch: PathBuf,
    pub logs: PathBuf,
}

/// Where one plugin lives (plugins spec §5.1). The package itself is under
/// `plugins_data_dir()` or wherever a directory source points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPaths {
    pub root: PathBuf,
    pub home: PathBuf,
    /// nono's own `$HOME`, as for agents (P2-5).
    pub nono_home: PathBuf,
    /// Key/value store (Phase 2 of Spec B); never granted to the sandbox.
    pub kv: PathBuf,
    /// Bulk state, read-write inside the sandbox; `BALERIX_PLUGIN_SCRATCH`.
    pub scratch: PathBuf,
    pub profile: PathBuf,
    pub launch: PathBuf,
    pub logs: PathBuf,
}

impl FleetPaths {
    /// A complete mise data dir; only `installs/` is exported to children.
    pub fn mise_pool(&self) -> PathBuf {
        self.root.join("mise")
    }
    /// sha256 of the `mise.toml` this pool was last installed from.
    pub fn installed_marker(&self) -> PathBuf {
        self.root.join("mise.installed")
    }
}

impl CrewPaths {
    pub fn mise_toml(&self) -> PathBuf {
        self.root.join("mise.toml")
    }
    pub fn mise_pool(&self) -> PathBuf {
        self.root.join("mise")
    }
    pub fn installed_marker(&self) -> PathBuf {
        self.root.join("mise.installed")
    }
}

impl StateLayout {
    /// XDG resolution: `$XDG_{STATE,DATA,CONFIG}_HOME/balerix`, defaulting to
    /// `~/.local/state`, `~/.local/share`, `~/.config`.
    pub fn from_env(home: &Path, env: impl Fn(&str) -> Option<OsString>) -> Self {
        let pick = |var: &str, default: PathBuf| {
            env(var)
                .map(PathBuf::from)
                .unwrap_or(default)
                .join("balerix")
        };
        Self {
            state_root: pick("XDG_STATE_HOME", home.join(".local").join("state")),
            data_root: pick("XDG_DATA_HOME", home.join(".local").join("share")),
            config_root: pick("XDG_CONFIG_HOME", home.join(".config")),
        }
    }

    pub fn mise_data_dir(&self) -> PathBuf {
        self.data_root.join("mise")
    }
    pub fn system_mise_toml(&self) -> PathBuf {
        self.config_root.join("mise.toml")
    }
    /// The generated copy of the system table that the daemon pool is
    /// installed from. `system_mise_toml()` is the admin's hand-written
    /// input; this is balerix's rendering of it, so the marker has a stable
    /// file to hash.
    pub fn system_mise_toml_generated(&self) -> PathBuf {
        self.data_root.join("mise.toml")
    }
    /// `server/`: token, vault key, endpoint, pid, log (Phase 3 spec §3.3, §5).
    pub fn server_dir(&self) -> PathBuf {
        self.state_root.join("server")
    }
    pub fn fleets_dir(&self) -> PathBuf {
        self.state_root.join("fleets")
    }
    pub fn fleet_dir(&self, f: &FleetName) -> PathBuf {
        self.fleets_dir().join(f.as_str())
    }
    pub fn fleet_gh_dir(&self, f: &FleetName) -> PathBuf {
        self.fleet_dir(f).join("gh")
    }
    pub fn crew(&self, c: &CrewRef) -> CrewPaths {
        let root = self.fleet_dir(&c.fleet).join("crews").join(c.crew.as_str());
        CrewPaths {
            repo: root.join("repo"),
            root,
        }
    }
    pub fn fleet(&self, f: &FleetName) -> FleetPaths {
        let root = self.fleet_dir(f);
        FleetPaths {
            mise_toml: root.join("mise.toml"),
            root,
        }
    }
    pub fn agent(&self, id: &AgentId) -> AgentPaths {
        let root = self
            .crew(&id.crew_ref())
            .root
            .join("agents")
            .join(id.agent.as_str());
        AgentPaths {
            home: root.join("home"),
            workspace: root.join("workspace"),
            nono_home: root.join("nono"),
            mise_toml: root.join("mise.toml"),
            profile: root.join("nono-profile.json"),
            launch: root.join("launch.sh"),
            logs: root.join("logs"),
            root,
        }
    }

    pub fn plugins_state_dir(&self) -> PathBuf {
        self.state_root.join("plugins")
    }
    /// Unpacked packages: `plugins/<name>/<digest12>/`.
    pub fn plugins_data_dir(&self) -> PathBuf {
        self.data_root.join("plugins")
    }
    pub fn plugin(&self, name: &AgentName) -> PluginPaths {
        let root = self.plugins_state_dir().join(name.as_str());
        PluginPaths {
            home: root.join("home"),
            nono_home: root.join("nono"),
            kv: root.join("kv"),
            scratch: root.join("scratch"),
            profile: root.join("nono-profile.json"),
            launch: root.join("launch.sh"),
            logs: root.join("logs"),
            root,
        }
    }
    /// sha256 of the system tool table the daemon pool was installed from.
    pub fn system_installed_marker(&self) -> PathBuf {
        self.data_root.join("mise.installed")
    }
    /// The read-only pools an agent's mise falls back through, nearest
    /// first: crew, fleet, daemon.
    pub fn agent_pools(&self, id: &AgentId) -> Vec<PathBuf> {
        vec![
            self.crew(&id.crew_ref()).mise_pool(),
            self.fleet(&id.fleet).mise_pool(),
            self.mise_data_dir(),
        ]
    }
    /// `MISE_SHARED_INSTALL_DIRS` for an agent: every pool's `installs/`,
    /// colon-separated. mise searches its own `MISE_DATA_DIR` first, then
    /// these in order, and never writes into them.
    pub fn shared_install_dirs(&self, id: &AgentId) -> String {
        shared_list(&self.agent_pools(id))
    }
}

impl PluginPaths {
    /// Holds the plugin hash last installed and validated; removed when the
    /// profile changes, rewritten after a successful install.
    pub fn installed_marker(&self) -> PathBuf {
        self.root.join(".installed")
    }
    pub fn tmp_dir(&self) -> PathBuf {
        self.home.join("tmp")
    }
    pub fn xdg_config(&self) -> PathBuf {
        self.home.join(".config")
    }
    pub fn xdg_data(&self) -> PathBuf {
        self.home.join(".local").join("share")
    }
    pub fn xdg_state(&self) -> PathBuf {
        self.home.join(".local").join("state")
    }
    pub fn xdg_cache(&self) -> PathBuf {
        self.home.join(".cache")
    }
    pub fn mise_config_dir(&self) -> PathBuf {
        self.xdg_config().join("mise")
    }
    pub fn mise_state_dir(&self) -> PathBuf {
        self.xdg_state().join("mise")
    }
    pub fn mise_cache_dir(&self) -> PathBuf {
        self.xdg_cache().join("mise")
    }
}

impl AgentPaths {
    /// Written after `mise install` and `nono profile validate` succeed for
    /// the current `mise.toml` + `nono-profile.json`; removed when either
    /// rendered file changes (Phase 3 spec §6.2).
    pub fn installed_marker(&self) -> PathBuf {
        self.root.join(".installed")
    }

    pub fn claude_dir(&self) -> PathBuf {
        self.home.join(".claude")
    }
    /// The agent's temp dir, 0700 inside `home/`. The sandbox grants nothing
    /// under `/tmp`, and claude refuses to start when its temp dir
    /// (`/tmp/claude-<uid>` by default) is unreachable, so `TMPDIR` and
    /// `CLAUDE_CODE_TMPDIR` point here.
    pub fn tmp_dir(&self) -> PathBuf {
        self.home.join("tmp")
    }
    pub fn claude_projects(&self) -> PathBuf {
        self.claude_dir().join("projects")
    }
    pub fn xdg_config(&self) -> PathBuf {
        self.home.join(".config")
    }
    pub fn xdg_data(&self) -> PathBuf {
        self.home.join(".local").join("share")
    }
    pub fn xdg_state(&self) -> PathBuf {
        self.home.join(".local").join("state")
    }
    pub fn xdg_cache(&self) -> PathBuf {
        self.home.join(".cache")
    }
    pub fn gh_dir(&self) -> PathBuf {
        self.xdg_config().join("gh")
    }
    pub fn mise_config_dir(&self) -> PathBuf {
        self.xdg_config().join("mise")
    }
    pub fn mise_state_dir(&self) -> PathBuf {
        self.xdg_state().join("mise")
    }
    pub fn mise_cache_dir(&self) -> PathBuf {
        self.xdg_cache().join("mise")
    }
    /// The agent's own `MISE_DATA_DIR`: writable, inside `home/`, so an
    /// agent can `mise install` a tool the fleet never declared without
    /// touching any shared pool (Spec E, E-2).
    pub fn mise_data_dir(&self) -> PathBuf {
        self.xdg_data().join("mise")
    }
}

/// Joins pool roots into a `MISE_SHARED_INSTALL_DIRS` value.
pub fn shared_list(pools: &[PathBuf]) -> String {
    pools
        .iter()
        .map(|p| p.join("installs").display().to_string())
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    #[test]
    fn defaults_to_xdg_dirs_under_home() {
        let l = StateLayout::from_env(Path::new("/h"), no_env);
        assert_eq!(l.state_root, PathBuf::from("/h/.local/state/balerix"));
        assert_eq!(l.data_root, PathBuf::from("/h/.local/share/balerix"));
        assert_eq!(l.config_root, PathBuf::from("/h/.config/balerix"));
    }

    #[test]
    fn honours_xdg_variables() {
        let l = StateLayout::from_env(Path::new("/h"), |k| {
            (k == "XDG_STATE_HOME").then(|| OsString::from("/xs"))
        });
        assert_eq!(l.state_root, PathBuf::from("/xs/balerix"));
        assert_eq!(l.data_root, PathBuf::from("/h/.local/share/balerix"));
    }

    #[test]
    fn agent_paths_follow_the_spec_layout() {
        let l = StateLayout::from_env(Path::new("/h"), no_env);
        let a = l.agent(&"payments/backend/alice".parse().unwrap());
        let base = "/h/.local/state/balerix/fleets/payments/crews/backend/agents/alice";
        assert_eq!(a.root, PathBuf::from(base));
        assert_eq!(a.home, PathBuf::from(format!("{base}/home")));
        assert_eq!(a.tmp_dir(), PathBuf::from(format!("{base}/home/tmp")));
        assert_eq!(a.nono_home, PathBuf::from(format!("{base}/nono")));
        assert_eq!(
            a.profile,
            PathBuf::from(format!("{base}/nono-profile.json"))
        );
        assert_eq!(
            a.claude_dir(),
            PathBuf::from(format!("{base}/home/.claude"))
        );
        assert_eq!(
            a.mise_cache_dir(),
            PathBuf::from(format!("{base}/home/.cache/mise"))
        );
        assert_eq!(
            l.crew(&"payments/backend".parse().unwrap()).repo,
            PathBuf::from("/h/.local/state/balerix/fleets/payments/crews/backend/repo")
        );
        assert_eq!(
            l.fleet_gh_dir(&"payments".parse().unwrap()),
            PathBuf::from("/h/.local/state/balerix/fleets/payments/gh")
        );
        assert_eq!(
            l.mise_data_dir(),
            PathBuf::from("/h/.local/share/balerix/mise")
        );
        assert_eq!(
            l.server_dir(),
            PathBuf::from("/h/.local/state/balerix/server")
        );
        assert_eq!(
            l.fleets_dir(),
            PathBuf::from("/h/.local/state/balerix/fleets")
        );
    }

    #[test]
    fn plugin_paths_live_under_plugins_in_state_and_packages_under_data() {
        let l = StateLayout::from_env(Path::new("/h"), no_env);
        assert_eq!(
            l.plugins_state_dir(),
            PathBuf::from("/h/.local/state/balerix/plugins")
        );
        assert_eq!(
            l.plugins_data_dir(),
            PathBuf::from("/h/.local/share/balerix/plugins")
        );
        let p = l.plugin(&"web".parse().unwrap());
        let base = "/h/.local/state/balerix/plugins/web";
        assert_eq!(p.root, PathBuf::from(base));
        assert_eq!(p.home, PathBuf::from(format!("{base}/home")));
        assert_eq!(p.nono_home, PathBuf::from(format!("{base}/nono")));
        assert_eq!(p.kv, PathBuf::from(format!("{base}/kv")));
        assert_eq!(p.scratch, PathBuf::from(format!("{base}/scratch")));
        assert_eq!(
            p.profile,
            PathBuf::from(format!("{base}/nono-profile.json"))
        );
        assert_eq!(p.launch, PathBuf::from(format!("{base}/launch.sh")));
        assert_eq!(p.logs, PathBuf::from(format!("{base}/logs")));
        assert_eq!(
            p.installed_marker(),
            PathBuf::from(format!("{base}/.installed"))
        );
        assert_eq!(p.tmp_dir(), PathBuf::from(format!("{base}/home/tmp")));
        assert_eq!(
            p.xdg_config(),
            PathBuf::from(format!("{base}/home/.config"))
        );
        assert_eq!(
            p.mise_state_dir(),
            PathBuf::from(format!("{base}/home/.local/state/mise"))
        );
    }

    #[test]
    fn pool_paths_hang_off_their_owner() {
        let l = StateLayout::from_env(Path::new("/h"), no_env);
        let fleet = l.fleet(&"payments".parse().unwrap());
        assert_eq!(
            fleet.root,
            PathBuf::from("/h/.local/state/balerix/fleets/payments")
        );
        assert_eq!(
            fleet.mise_toml,
            PathBuf::from("/h/.local/state/balerix/fleets/payments/mise.toml")
        );
        assert_eq!(
            fleet.mise_pool(),
            PathBuf::from("/h/.local/state/balerix/fleets/payments/mise")
        );
        assert_eq!(
            fleet.installed_marker(),
            PathBuf::from("/h/.local/state/balerix/fleets/payments/mise.installed")
        );

        let crew = l.crew(&"payments/backend".parse().unwrap());
        let base = "/h/.local/state/balerix/fleets/payments/crews/backend";
        assert_eq!(crew.mise_toml(), PathBuf::from(format!("{base}/mise.toml")));
        assert_eq!(crew.mise_pool(), PathBuf::from(format!("{base}/mise")));
        assert_eq!(
            crew.installed_marker(),
            PathBuf::from(format!("{base}/mise.installed"))
        );

        let a = l.agent(&"payments/backend/alice".parse().unwrap());
        assert_eq!(
            a.mise_data_dir(),
            PathBuf::from(format!("{base}/agents/alice/home/.local/share/mise")),
            "the agent's own installs live inside its writable home"
        );
        assert_eq!(
            l.system_installed_marker(),
            PathBuf::from("/h/.local/share/balerix/mise.installed")
        );
    }

    #[test]
    fn agent_pools_run_nearest_first_and_join_into_the_shared_list() {
        let l = StateLayout::from_env(Path::new("/h"), no_env);
        let id: AgentId = "payments/backend/alice".parse().unwrap();
        let state = "/h/.local/state/balerix/fleets/payments";
        assert_eq!(
            l.agent_pools(&id),
            vec![
                PathBuf::from(format!("{state}/crews/backend/mise")),
                PathBuf::from(format!("{state}/mise")),
                PathBuf::from("/h/.local/share/balerix/mise"),
            ],
            "crew beats fleet beats daemon"
        );
        assert_eq!(
            l.shared_install_dirs(&id),
            format!(
                "{state}/crews/backend/mise/installs:{state}/mise/installs:\
                 /h/.local/share/balerix/mise/installs"
            )
        );
    }
}
