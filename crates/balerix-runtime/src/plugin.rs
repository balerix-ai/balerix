//! Materializing a plugin (plugins spec §5.1): the sandboxed home, the
//! profile, `launch.sh` running `nono run → mise run <start>` from the
//! package root, and the daemon-side `mise install`.

use std::collections::BTreeMap;
use std::path::Path;

use balerix_core::{AgentName, HookTarget, LaunchPlan, MaterializeError, ResolvedPlugin};

use crate::fsutil::{ensure_dir, ensure_private_dir, write_atomic};
use crate::launch::{hooks_port, outer_path};
use crate::layout::{PluginPaths, StateLayout};
use crate::materializer::{RenderOutcome, Runtime};
use crate::quote::sh_quote;
use crate::sandbox::{Grants, SYSTEM_READ, render_profile, validate_profile_at, write_profile_at};
use crate::tools::{Cmd, ToolPaths};

/// Where mise's searches for config files must stop, colon-separated. Both
/// ceilings are exclusive — mise scans neither the ceiling directory itself
/// nor anything above it:
/// - the package's parent, so the walk up from the cwd `launch.sh` sets
///   finds the package's own `mise.toml` and nothing else;
/// - the plugin's home, because `mise run` also collects config files from
///   every ancestor of `$HOME` when it works out which tools a task needs.
///   Unbounded, that second walk climbs out of the state root and dies on
///   the first config file the sandbox does not grant ("error parsing
///   config file: … Permission denied", mise 2026.9.1).
fn mise_ceiling(plugin: &ResolvedPlugin, paths: &PluginPaths) -> String {
    let above_package = plugin.package.parent().unwrap_or(&plugin.package);
    format!("{}:{}", above_package.display(), paths.home.display())
}

/// The plugin's environment: the nono profile's `set_vars` (spec §5.1).
pub fn plugin_env(
    plugin: &ResolvedPlugin,
    paths: &PluginPaths,
    layout: &StateLayout,
    api_url: &str,
    token: &str,
) -> BTreeMap<String, String> {
    let s = |p: &Path| p.display().to_string();
    BTreeMap::from([
        ("HOME".to_string(), s(&paths.home)),
        ("XDG_CONFIG_HOME".to_string(), s(&paths.xdg_config())),
        ("XDG_DATA_HOME".to_string(), s(&paths.xdg_data())),
        ("XDG_STATE_HOME".to_string(), s(&paths.xdg_state())),
        ("XDG_CACHE_HOME".to_string(), s(&paths.xdg_cache())),
        ("TMPDIR".to_string(), s(&paths.tmp_dir())),
        // No `MISE_GLOBAL_CONFIG_FILE` here, unlike the daemon-side install
        // below: mise runs a task in the root of the config that defines it,
        // and the config named by that variable is the *global* one, whose
        // root is `$HOME` (mise 2026.9.1). Naming the package's `mise.toml`
        // there would start every plugin in its own home instead of its
        // package, so `run = "node server.js"` would not find its own code.
        // Left unset, the file `launch.sh`'s `cd` puts in the cwd is found
        // by the ordinary upward walk as a local config, and the start task
        // runs in the package.
        (
            "MISE_CEILING_PATHS".to_string(),
            mise_ceiling(plugin, paths),
        ),
        ("MISE_DATA_DIR".to_string(), s(&layout.mise_data_dir())),
        ("MISE_CONFIG_DIR".to_string(), s(&paths.mise_config_dir())),
        ("MISE_STATE_DIR".to_string(), s(&paths.mise_state_dir())),
        ("MISE_CACHE_DIR".to_string(), s(&paths.mise_cache_dir())),
        // never install from inside the sandbox; the daemon did already
        ("MISE_AUTO_INSTALL".to_string(), "false".to_string()),
        ("BALERIX_API_URL".to_string(), api_url.to_string()),
        ("BALERIX_PLUGIN_NAME".to_string(), plugin.name.to_string()),
        ("BALERIX_PLUGIN_TOKEN".to_string(), token.to_string()),
        ("BALERIX_PLUGIN_SCRATCH".to_string(), s(&paths.scratch)),
    ])
}

/// Read: system dirs, the shared mise install dir, the package, the
/// balerix and mise binaries. Read-write: `home/` and `scratch/`. `kv/` is
/// reached through the API only, so it is never granted.
pub fn plugin_grants(
    plugin: &ResolvedPlugin,
    paths: &PluginPaths,
    layout: &StateLayout,
    balerix: &Path,
    mise: &Path,
) -> Grants {
    let mut read: Vec<std::path::PathBuf> = SYSTEM_READ.iter().map(Into::into).collect();
    read.push(layout.mise_data_dir());
    read.push(plugin.package.clone());
    read.push(balerix.to_path_buf());
    read.push(std::fs::canonicalize(mise).unwrap_or_else(|_| mise.to_path_buf()));
    Grants {
        read,
        allow: vec![paths.home.clone(), paths.scratch.clone()],
    }
}

/// `launch.sh` and the `LaunchPlan`: `cd <package> && exec env -i … nono
/// run --profile … -- mise run <start>`. No secret is an input here.
pub fn render_plugin_launch(
    plugin: &ResolvedPlugin,
    paths: &PluginPaths,
    tools: &ToolPaths,
) -> (String, LaunchPlan) {
    let env = BTreeMap::from([
        ("PATH".to_string(), outer_path(tools)),
        ("HOME".to_string(), paths.nono_home.display().to_string()),
    ]);
    let argv: Vec<String> = vec![
        tools.nono.display().to_string(),
        "-s".into(),
        "--log-file".into(),
        paths.logs.join("nono.log").display().to_string(),
        "run".into(),
        "--profile".into(),
        paths.profile.display().to_string(),
        "--".into(),
        tools.mise.display().to_string(),
        "run".into(),
        plugin.manifest.start.clone(),
    ];
    let mut script = format!(
        "#!/bin/sh\n# generated by balerix for plugin {} — safe to run by hand\ncd {} && exec env -i",
        plugin.name,
        sh_quote(&plugin.package.display().to_string())
    );
    for (k, v) in &env {
        script.push_str(&format!(" {k}={}", sh_quote(v)));
    }
    script.push_str(" \\\n ");
    for a in &argv {
        script.push(' ');
        script.push_str(&sh_quote(a));
    }
    script.push('\n');
    (
        script,
        LaunchPlan {
            cwd: plugin.package.clone(),
            env,
            argv,
            script: paths.launch.clone(),
        },
    )
}

fn io(name: &AgentName, path: &Path, e: std::io::Error) -> MaterializeError {
    MaterializeError::Io {
        id: balerix_core::plugin_id(name).to_string(),
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// `home/` (0700) with its XDG tree and 0700 `tmp/`, `kv/` (0700),
/// `scratch/`, `logs/`, nono's home. Idempotent.
pub fn write_plugin_home(name: &AgentName, paths: &PluginPaths) -> Result<(), MaterializeError> {
    for d in [&paths.home, &paths.tmp_dir(), &paths.kv] {
        ensure_private_dir(d).map_err(|e| io(name, d, e))?;
    }
    for d in [
        paths.xdg_config(),
        paths.xdg_data(),
        paths.xdg_state(),
        paths.xdg_cache(),
        paths.mise_config_dir(),
        paths.mise_state_dir(),
        paths.mise_cache_dir(),
        paths.scratch.clone(),
        paths.logs.clone(),
        paths.nono_home.clone(),
    ] {
        ensure_dir(&d).map_err(|e| io(name, &d, e))?;
    }
    Ok(())
}

/// Daemon-side `mise trust <package>/mise.toml` then `mise install`, with
/// the same `MISE_{DATA,CONFIG,STATE,CACHE}_DIR` the sandbox will see, so
/// the trust record and the installed tools land where the sandboxed run
/// looks for them. `cwd=/` as for agents, so the package's config is named
/// outright here rather than discovered.
pub fn install_plugin_tools(
    tools: &ToolPaths,
    layout: &StateLayout,
    plugin: &ResolvedPlugin,
    paths: &PluginPaths,
) -> Result<(), MaterializeError> {
    let id = plugin.id().to_string();
    let s = |p: &Path| p.display().to_string();
    let env = BTreeMap::from([
        (
            "MISE_GLOBAL_CONFIG_FILE".to_string(),
            s(&plugin.package.join("mise.toml")),
        ),
        ("MISE_DATA_DIR".to_string(), s(&layout.mise_data_dir())),
        ("MISE_CONFIG_DIR".to_string(), s(&paths.mise_config_dir())),
        ("MISE_STATE_DIR".to_string(), s(&paths.mise_state_dir())),
        ("MISE_CACHE_DIR".to_string(), s(&paths.mise_cache_dir())),
        ("MISE_YES".to_string(), "1".to_string()),
        ("MISE_QUIET".to_string(), "1".to_string()),
        ("MISE_AUTO_INSTALL".to_string(), "false".to_string()),
    ]);
    let log = paths.logs.join("mise.toolchain.log");
    let run = |args: &[&str]| {
        Cmd::new(&tools.mise)
            .args(args.iter().copied())
            .envs(&env)
            .cwd(Path::new("/"))
            .log(&log)
            .run()
            .map(|_| ())
            .map_err(|f| MaterializeError::Tool {
                id: id.clone(),
                tool: f.tool,
                subcommand: f.subcommand,
                args: f.args,
                stderr: f.stderr,
            })
    };
    run(&["trust", &s(&plugin.package.join("mise.toml"))])?;
    run(&["install"])
}

impl Runtime {
    /// The file half: home, profile, `launch.sh`. `toolchain_changed` is
    /// true when the profile changed or the marker does not hold this
    /// plugin's hash; the caller then runs `install_plugin_tools` and
    /// `validate_profile_at`.
    pub fn render_plugin(
        &self,
        plugin: &ResolvedPlugin,
        host: &HookTarget,
    ) -> Result<RenderOutcome, MaterializeError> {
        let id = plugin.id();
        let paths = self.layout.plugin(&plugin.name);
        write_plugin_home(&plugin.name, &paths)?;
        let env = plugin_env(plugin, &paths, &self.layout, &host.url, &host.secret);
        let profile = render_profile(
            &id,
            &plugin_grants(
                plugin,
                &paths,
                &self.layout,
                &self.tools.balerix,
                &self.tools.mise,
            ),
            hooks_port(&host.url),
            &env,
            &plugin.manifest.sandbox,
        )?;
        let profile_changed = write_profile_at(&id, &paths.profile, &profile)?;
        let installed = std::fs::read_to_string(paths.installed_marker()).unwrap_or_default();
        let toolchain_changed = profile_changed || installed.trim() != plugin.hash().as_str();
        if toolchain_changed
            && let Err(e) = std::fs::remove_file(paths.installed_marker())
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(io(&plugin.name, &paths.installed_marker(), e));
        }
        let (script, plan) = render_plugin_launch(plugin, &paths, &self.tools);
        write_atomic(&paths.launch, script.as_bytes(), 0o755)
            .map_err(|e| io(&plugin.name, &paths.launch, e))?;
        Ok(RenderOutcome {
            plan,
            toolchain_changed,
        })
    }

    /// `mise install` + `nono profile validate` unless the marker holds the
    /// current hash; writes the hash on success.
    pub fn install_plugin(&self, plugin: &ResolvedPlugin) -> Result<(), MaterializeError> {
        let paths = self.layout.plugin(&plugin.name);
        let marker = paths.installed_marker();
        if std::fs::read_to_string(&marker)
            .map(|s| s.trim() == plugin.hash().as_str())
            .unwrap_or(false)
        {
            return Ok(());
        }
        install_plugin_tools(&self.tools, &self.layout, plugin, &paths)?;
        validate_profile_at(
            &self.tools,
            &plugin.id(),
            &paths.profile,
            &paths.nono_home,
            &paths.logs.join("nono.validate.log"),
        )?;
        write_atomic(&marker, plugin.hash().as_str().as_bytes(), 0o644)
            .map_err(|e| io(&plugin.name, &marker, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::StateLayout;
    use balerix_api::PluginManifest;
    use serde_json::json;
    use std::path::Path;

    fn plugin() -> ResolvedPlugin {
        let manifest: PluginManifest = serde_json::from_value(json!({
            "apiVersion": "balerix/v1", "kind": "Plugin", "name": "web",
            "version": "0.1.0", "protocol": 1, "start": "serve",
            "sandbox": { "network": { "block": true } }
        }))
        .unwrap();
        ResolvedPlugin {
            name: "web".parse().unwrap(),
            package: "/data/plugins/web/abc123def456".into(),
            manifest,
            config: json!({}),
            digest: Some("abc123def456ffff".into()),
        }
    }

    fn tools() -> ToolPaths {
        ToolPaths {
            git: "/usr/bin/git".into(),
            gh: "/opt/gh".into(),
            mise: "/opt/mise/bin/mise".into(),
            nono: "/opt/nono".into(),
            tmux: "/opt/tmux".into(),
            balerix: "/opt/balerix".into(),
        }
    }

    #[test]
    fn env_isolates_home_points_mise_at_the_package_and_carries_the_token() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let p = plugin();
        let paths = layout.plugin(&p.name);
        let env = plugin_env(&p, &paths, &layout, "http://127.0.0.1:7643", "tok-1");
        let base = "/h/.local/state/balerix/plugins/web";
        assert_eq!(env["HOME"], format!("{base}/home"));
        assert_eq!(env["TMPDIR"], format!("{base}/home/tmp"));
        assert_eq!(env["XDG_STATE_HOME"], format!("{base}/home/.local/state"));
        assert!(
            !env.contains_key("MISE_GLOBAL_CONFIG_FILE"),
            "the package's config must be local, or its tasks run in $HOME"
        );
        assert_eq!(
            env["MISE_CEILING_PATHS"],
            format!("/data/plugins/web:{base}/home"),
            "stop above the package and at the home mise walks up from"
        );
        assert_eq!(env["MISE_DATA_DIR"], "/h/.local/share/balerix/mise");
        assert_eq!(env["MISE_AUTO_INSTALL"], "false");
        assert_eq!(env["BALERIX_API_URL"], "http://127.0.0.1:7643");
        assert_eq!(env["BALERIX_PLUGIN_NAME"], "web");
        assert_eq!(env["BALERIX_PLUGIN_TOKEN"], "tok-1");
        assert_eq!(env["BALERIX_PLUGIN_SCRATCH"], format!("{base}/scratch"));
        assert!(!env.contains_key("PATH"), "PATH is nono's");
        assert!(!env.contains_key("CLAUDE_CONFIG_DIR"), "no claude here");
        assert_eq!(env.len(), 16);
    }

    #[test]
    fn grants_read_the_package_and_write_only_home_and_scratch() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let p = plugin();
        let paths = layout.plugin(&p.name);
        let g = plugin_grants(
            &p,
            &paths,
            &layout,
            Path::new("/opt/balerix"),
            Path::new("/opt/mise"),
        );
        assert_eq!(g.read[0], Path::new("/usr"));
        assert_eq!(g.read[5], Path::new("/h/.local/share/balerix/mise"));
        assert_eq!(g.read[6], Path::new("/data/plugins/web/abc123def456"));
        assert_eq!(g.read[7], Path::new("/opt/balerix"));
        assert_eq!(g.read[8], Path::new("/opt/mise"));
        assert_eq!(g.read.len(), 9);
        assert_eq!(
            g.allow,
            vec![paths.home.clone(), paths.scratch.clone()],
            "kv is never granted"
        );
    }

    #[test]
    fn launch_runs_the_start_task_from_the_package_root() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let p = plugin();
        let paths = layout.plugin(&p.name);
        let (script, plan) = render_plugin_launch(&p, &paths, &tools());
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains(
            "cd '/data/plugins/web/abc123def456' && exec env -i HOME='/h/.local/state/balerix/plugins/web/nono' PATH='/usr/local/bin:/usr/bin:/bin:/opt/mise/bin'"
        ));
        assert!(script.contains(
            "'run' '--profile' '/h/.local/state/balerix/plugins/web/nono-profile.json' '--' '/opt/mise/bin/mise' 'run' 'serve'\n"
        ));
        assert!(!script.contains("tok"), "no token input exists here");
        assert_eq!(plan.cwd, Path::new("/data/plugins/web/abc123def456"));
        assert_eq!(plan.script, paths.launch);
        assert_eq!(plan.argv[0], "/opt/nono");
        assert_eq!(plan.argv.last().unwrap(), "serve");
        assert_eq!(plan.env.len(), 2);
    }

    #[test]
    fn home_is_private_and_has_the_xdg_tree() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let layout = StateLayout::from_env(dir.path(), |_| None);
        let p = plugin();
        let paths = layout.plugin(&p.name);
        write_plugin_home(&p.name, &paths).unwrap();
        let mode = |q: &Path| std::fs::metadata(q).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&paths.home), 0o700);
        assert_eq!(mode(&paths.tmp_dir()), 0o700);
        assert_eq!(mode(&paths.kv), 0o700);
        for d in [
            paths.xdg_config(),
            paths.xdg_data(),
            paths.xdg_state(),
            paths.xdg_cache(),
            paths.scratch.clone(),
            paths.logs.clone(),
            paths.nono_home.clone(),
        ] {
            assert!(d.is_dir(), "{}", d.display());
        }
        write_plugin_home(&p.name, &paths).unwrap(); // idempotent
    }
}
