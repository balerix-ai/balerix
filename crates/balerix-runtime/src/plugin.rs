//! Materializing a plugin (plugins spec §5.1): the sandboxed home, the
//! profile, `launch.sh` running `agent-supervise → nono run → mise run
//! <start>` from the package root, and the daemon-side `mise install`.

use std::collections::BTreeMap;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use balerix_core::{AgentName, HookTarget, LaunchPlan, MaterializeError, ResolvedPlugin};

use crate::fsutil::{
    DirMode, ensure_dir, ensure_path_at, ensure_private_dir, open_dir_root, relative_to,
    write_atomic,
};
use crate::install_sandbox::{InstallCall, InstallEnv, SandboxedInstall, Scope};
use crate::launch::{hooks_port, outer_path};
use crate::layout::{PluginPaths, StateLayout};
use crate::materializer::{RenderOutcome, Runtime};
use crate::quote::sh_quote;
use crate::sandbox::{
    Grants, PluginGrants, SYSTEM_READ, check_plugin_sandbox, render_profile, validate_profile_at,
    write_profile_at,
};
use crate::socket_policy::{SocketPolicy, apply_to_profile, wrap_command};
use crate::tools::ToolPaths;

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
///
/// A package at a filesystem root has no directory above it to stop at,
/// and the package itself as the ceiling would hide its own `mise.toml`:
/// an error rather than that wrong value (#4). A `:` in either path would
/// split the list; the daemon refuses such a package when it resolves it.
fn mise_ceiling(plugin: &ResolvedPlugin, paths: &PluginPaths) -> Result<String, MaterializeError> {
    let above_package = plugin
        .package
        .parent()
        .ok_or_else(|| MaterializeError::Invalid {
            id: plugin.id().to_string(),
            message: format!(
                "package {} has no parent directory for mise to stop at",
                plugin.package.display()
            ),
        })?;
    Ok(format!(
        "{}:{}",
        above_package.display(),
        paths.home.display()
    ))
}

/// The plugin's environment: the nono profile's `set_vars` (spec §5.1).
pub fn plugin_env(
    plugin: &ResolvedPlugin,
    paths: &PluginPaths,
    layout: &StateLayout,
    api_url: &str,
    token: &str,
) -> Result<BTreeMap<String, String>, MaterializeError> {
    let s = |p: &Path| p.display().to_string();
    Ok(BTreeMap::from([
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
            mise_ceiling(plugin, paths)?,
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
    ]))
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

/// `launch.sh` and the `LaunchPlan`: `cd <package> && exec env -i …
/// balerix agent-supervise -- nono run --profile … -- mise run <start>`.
/// The supervisor stands in front of nono as it does for an agent (Spec N
/// amendment §13.4, #119), so a plugin's stop and restart end its whole
/// process tree. No secret is an input here.
pub fn render_plugin_launch(
    plugin: &ResolvedPlugin,
    paths: &PluginPaths,
    tools: &ToolPaths,
    policy: SocketPolicy,
) -> (String, LaunchPlan) {
    let env = BTreeMap::from([
        ("PATH".to_string(), outer_path(tools)),
        ("HOME".to_string(), paths.nono_home.display().to_string()),
    ]);
    let mut argv: Vec<String> = vec![
        tools.balerix.display().to_string(),
        "agent-supervise".into(),
        "--".into(),
        tools.nono.display().to_string(),
        "-s".into(),
        "--log-file".into(),
        paths.logs.join("nono.log").display().to_string(),
        "run".into(),
        "--profile".into(),
        paths.profile.display().to_string(),
        "--".into(),
    ];
    argv.extend(wrap_command(
        policy,
        &tools.balerix,
        vec![
            tools.mise.display().to_string(),
            "run".into(),
            plugin.manifest.start.clone(),
        ],
    ));
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
/// `scratch/`, `logs/`, nono's home. Idempotent. As for an agent's home
/// (`home::write_home`), every directory under `home/` is reached by
/// descriptor without following a link below `home/` itself.
pub fn write_plugin_home(name: &AgentName, paths: &PluginPaths) -> Result<(), MaterializeError> {
    ensure_private_dir(&paths.kv).map_err(|e| io(name, &paths.kv, e))?;
    for d in [&paths.scratch, &paths.logs, &paths.nono_home] {
        ensure_dir(d).map_err(|e| io(name, d, e))?;
    }
    let home =
        open_dir_root(&paths.home, DirMode::Private).map_err(|e| io(name, &paths.home, e))?;
    let sub = |path: &Path, mode: DirMode| {
        relative_to(&paths.home, path)
            .and_then(|rel| ensure_path_at(home.as_fd(), rel, mode))
            .map(drop)
            .map_err(|e| io(name, path, e))
    };
    sub(&paths.tmp_dir(), DirMode::Private)?;
    for d in [
        paths.xdg_config(),
        paths.xdg_data(),
        paths.xdg_state(),
        paths.xdg_cache(),
        paths.mise_config_dir(),
        paths.mise_state_dir(),
        paths.mise_cache_dir(),
    ] {
        sub(&d, DirMode::Default)?;
    }
    Ok(())
}

/// The mise variables both daemon calls for one plugin share: the
/// package's `mise.toml` as the global config, the daemon pool as the data
/// dir, the sandbox's ceiling, `config`/`state`/`cache` as given.
fn plugin_mise_vars(
    plugin: &ResolvedPlugin,
    paths: &PluginPaths,
    layout: &StateLayout,
    [config, state, cache]: [PathBuf; 3],
) -> Result<BTreeMap<String, String>, MaterializeError> {
    let s = |p: &Path| p.display().to_string();
    Ok(BTreeMap::from([
        (
            "MISE_GLOBAL_CONFIG_FILE".to_string(),
            s(&plugin.package.join("mise.toml")),
        ),
        ("MISE_DATA_DIR".to_string(), s(&layout.mise_data_dir())),
        ("MISE_CONFIG_DIR".to_string(), s(&config)),
        ("MISE_STATE_DIR".to_string(), s(&state)),
        ("MISE_CACHE_DIR".to_string(), s(&cache)),
        (
            "MISE_CEILING_PATHS".to_string(),
            mise_ceiling(plugin, paths)?,
        ),
        ("MISE_YES".to_string(), "1".to_string()),
        ("MISE_QUIET".to_string(), "1".to_string()),
        ("MISE_AUTO_INSTALL".to_string(), "false".to_string()),
    ]))
}

fn system_and_mise(mise: &Path) -> Vec<PathBuf> {
    let mut read: Vec<PathBuf> = SYSTEM_READ.iter().map(Into::into).collect();
    read.push(std::fs::canonicalize(mise).unwrap_or_else(|_| mise.to_path_buf()));
    read
}

/// The daemon's `mise install` for one plugin (`install_sandbox`). Read:
/// the system prefixes, the `mise` binary (canonical) and the package,
/// whose `mise.toml` mise reads. Read-write: the daemon pool, the plugin's
/// `MISE_DATA_DIR` (its tools are installed there, beside the system
/// table's, as before), and the daemon-owned `mise-state/` holding mise's
/// config, state and cache for it (`PluginPaths::mise_install_state`).
/// Nothing the plugin can write: a link it left in its `home/` would
/// otherwise carry a write into the pool.
pub fn plugin_install_call(
    plugin: &ResolvedPlugin,
    paths: &PluginPaths,
    layout: &StateLayout,
    mise: &Path,
) -> Result<InstallCall, MaterializeError> {
    let mut read = system_and_mise(mise);
    read.push(plugin.package.clone());
    let state = paths.mise_install_state();
    Ok(InstallCall {
        name: format!("balerix-mise-plugin-{}", plugin.name),
        description: format!(
            "generated by balerix for its own mise install for plugin {}",
            plugin.name
        ),
        grants: Grants {
            read,
            allow: vec![layout.mise_data_dir(), state.clone()],
        },
        vars: plugin_mise_vars(
            plugin,
            paths,
            layout,
            [
                state.join("config"),
                state.join("state"),
                state.join("cache"),
            ],
        )?,
        scope: Scope::Owned,
    })
}

/// The daemon's `mise trust` for one plugin: the trust record goes to the
/// plugin's own `MISE_STATE_DIR`, where its sandboxed `mise run` looks for
/// it (`plugin_env`). Read: the system prefixes, the `mise` binary, the
/// package and the daemon pool. Read-write: the plugin's `home/` itself,
/// never a directory below it, which the plugin could replace with a link
/// that nono would follow when it grants; `home/` sits in the plugin root,
/// which no sandbox can write. Never the pool.
pub fn plugin_trust_call(
    plugin: &ResolvedPlugin,
    paths: &PluginPaths,
    layout: &StateLayout,
    mise: &Path,
) -> Result<InstallCall, MaterializeError> {
    let mut read = system_and_mise(mise);
    read.push(plugin.package.clone());
    read.push(layout.mise_data_dir());
    Ok(InstallCall {
        name: format!("balerix-mise-trust-plugin-{}", plugin.name),
        description: format!(
            "generated by balerix for its own mise trust for plugin {}",
            plugin.name
        ),
        grants: Grants {
            read,
            allow: vec![paths.home.clone()],
        },
        vars: plugin_mise_vars(
            plugin,
            paths,
            layout,
            [
                paths.mise_config_dir(),
                paths.mise_state_dir(),
                paths.mise_cache_dir(),
            ],
        )?,
        scope: Scope::Owned,
    })
}

/// Daemon-side `mise trust <package>/mise.toml` then `mise install`, each
/// inside nono under a profile of its own (`plugin_trust_call`,
/// `plugin_install_call`): no call writes both the plugin's `home/` and
/// the daemon pool. `cwd=/` as for agents, so the package's config is
/// named outright here rather than discovered; the same
/// `MISE_CEILING_PATHS` as the sandbox, so no config above the package or
/// the plugin's home is read on the way (#4).
pub fn install_plugin_tools(
    tools: &ToolPaths,
    layout: &StateLayout,
    plugin: &ResolvedPlugin,
    paths: &PluginPaths,
    policy: SocketPolicy,
    env: &InstallEnv,
) -> Result<(), MaterializeError> {
    // `home/` and the mise directories below it, made again without
    // following links just before mise runs (the plugin can write them);
    // the pool and the install's state: a grant must name a directory
    // that exists when the sandbox starts
    write_plugin_home(&plugin.name, paths)?;
    for dir in [layout.mise_data_dir(), paths.mise_install_state()] {
        std::fs::create_dir_all(&dir).map_err(|e| io(&plugin.name, &dir, e))?;
    }
    SandboxedInstall {
        tools,
        policy,
        env,
        id: &plugin.id().to_string(),
        scratch_in: &paths.root,
        log: &paths.logs.join("mise.toolchain.log"),
        nono_log: &paths.logs.join("nono-mise.log"),
        timeout: None,
        shared: &[],
    }
    .run(
        &plugin_trust_call(plugin, paths, layout, &tools.mise)?,
        &plugin_install_call(plugin, paths, layout, &tools.mise)?,
        &plugin.package.join("mise.toml"),
    )
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
        let env = plugin_env(plugin, &paths, &self.layout, &host.url, &host.secret)?;
        // before anything is written from the manifest's block (#2)
        let layout = &self.layout;
        self.with_socket_dirs(|sockets| {
            check_plugin_sandbox(
                &id,
                &plugin.manifest.sandbox,
                &[
                    ("state", &layout.state_root),
                    ("data", &layout.data_root),
                    ("config", &layout.config_root),
                ],
                sockets,
                PluginGrants {
                    read_only: &[plugin.package.clone(), layout.mise_data_dir()],
                    read_write: &[paths.home.clone(), paths.scratch.clone()],
                },
            )
        })?;
        // the policy last, over the merged manifest block, so nothing
        // the block carries can re-set what it pins
        let profile = apply_to_profile(
            render_profile(
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
            )?,
            self.socket_policy,
            &[paths.home.clone(), paths.scratch.clone()],
            &self.tools.balerix,
        );
        let profile_changed = write_profile_at(&id, &paths.profile, &profile)?;
        let installed = std::fs::read_to_string(paths.installed_marker()).unwrap_or_default();
        let toolchain_changed = profile_changed || installed.trim() != plugin.hash().as_str();
        if toolchain_changed
            && let Err(e) = std::fs::remove_file(paths.installed_marker())
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(io(&plugin.name, &paths.installed_marker(), e));
        }
        let (script, plan) = render_plugin_launch(plugin, &paths, &self.tools, self.socket_policy);
        write_atomic(&paths.launch, script.as_bytes(), 0o755)
            .map_err(|e| io(&plugin.name, &paths.launch, e))?;
        Ok(RenderOutcome {
            plan,
            toolchain_changed,
        })
    }

    /// `mise install` + `nono profile validate` when `render_plugin` said
    /// `toolchain_changed` (it compared the marker with the hash, and
    /// removed a stale one); writes the hash on success.
    pub fn install_plugin(
        &self,
        plugin: &ResolvedPlugin,
        toolchain_changed: bool,
    ) -> Result<(), MaterializeError> {
        if !toolchain_changed {
            return Ok(());
        }
        let paths = self.layout.plugin(&plugin.name);
        let marker = paths.installed_marker();
        install_plugin_tools(
            &self.tools,
            &self.layout,
            plugin,
            &paths,
            self.socket_policy,
            &self.install_env,
        )?;
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
            fleet_defaults: json!({}),
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
        let env = plugin_env(&p, &paths, &layout, "http://127.0.0.1:7643", "tok-1").unwrap();
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
    fn a_package_at_a_filesystem_root_has_no_ceiling() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let mut p = plugin();
        p.package = "/".into();
        let paths = layout.plugin(&p.name);
        let e = plugin_env(&p, &paths, &layout, "http://127.0.0.1:7643", "t").unwrap_err();
        assert_eq!(
            e.to_string(),
            "balerix/plugins/web: package / has no parent directory for mise to stop at"
        );
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
        let (script, plan) = render_plugin_launch(&p, &paths, &tools(), SocketPolicy::Open);
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
        assert!(
            script.contains("'/opt/balerix' 'agent-supervise' '--' '/opt/nono' '-s' '--log-file'"),
            "{script}"
        );
        assert_eq!(
            plan.argv[..4],
            ["/opt/balerix", "agent-supervise", "--", "/opt/nono"].map(String::from)
        );
        assert_eq!(plan.argv.last().unwrap(), "serve");
        assert_eq!(plan.env.len(), 2);
    }

    #[test]
    fn deny_runs_the_start_task_under_sandbox_exec_inside_nono() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let p = plugin();
        let paths = layout.plugin(&p.name);
        let render = |policy| render_plugin_launch(&p, &paths, &tools(), policy);
        let (script, plan) = render(SocketPolicy::Deny);
        // nono's `--` is the word after `--profile <path>`
        let after = plan.argv.iter().position(|a| a == "--profile").unwrap() + 3;
        assert_eq!(plan.argv[after - 1], "--");
        assert_eq!(
            plan.argv[after..],
            [
                "/opt/balerix",
                "sandbox-exec",
                "--",
                "/opt/mise/bin/mise",
                "run",
                "serve"
            ]
            .map(String::from)
        );
        assert!(script.contains("'sandbox-exec'"), "{script}");
        let (_, mediated) = render(SocketPolicy::Mediate);
        let (open_script, open) = render(SocketPolicy::Open);
        assert_eq!(mediated, open, "only Deny changes the command");
        assert!(!open_script.contains("sandbox-exec"));
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

    #[test]
    fn plugin_home_writes_never_follow_a_link() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let layout = StateLayout::from_env(dir.path(), |_| None);
        let p = plugin();
        let paths = layout.plugin(&p.name);
        let mode = |q: &Path| std::fs::symlink_metadata(q).unwrap().permissions().mode() & 0o777;
        for (i, planted) in [
            paths.tmp_dir(),
            paths.xdg_config(),
            paths.mise_config_dir(),
            paths.mise_state_dir(),
            paths.xdg_cache(),
        ]
        .into_iter()
        .enumerate()
        {
            write_plugin_home(&p.name, &paths).unwrap();
            let outside = dir.path().join(format!("outside-{i}"));
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("sentinel"), "left alone").unwrap();
            std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::remove_dir_all(&planted).unwrap();
            std::os::unix::fs::symlink(&outside, &planted).unwrap();
            write_plugin_home(&p.name, &paths).unwrap();
            assert_eq!(mode(&outside), 0o755);
            let names: Vec<_> = std::fs::read_dir(&outside)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names, ["sentinel"]);
            assert!(
                std::fs::symlink_metadata(&planted).unwrap().is_dir(),
                "{} is a real directory again",
                planted.display()
            );
            assert_eq!(mode(&paths.tmp_dir()), 0o700);
            for d in [
                paths.mise_config_dir(),
                paths.mise_state_dir(),
                paths.mise_cache_dir(),
            ] {
                assert!(std::fs::symlink_metadata(&d).unwrap().is_dir());
            }
        }
    }

    /// The plugin's two profiles, exactly. The install reads the system
    /// prefixes, the mise binary and the package, and writes the daemon
    /// pool (its `MISE_DATA_DIR`) and the daemon-owned `mise-state/`, none
    /// the plugin can write; the trust also reads the pool and writes the
    /// plugin's `home/`, where its trust record goes, and not the pool.
    #[test]
    fn the_plugin_install_and_trust_profiles_never_write_both_home_and_the_pool() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let p = plugin();
        let paths = layout.plugin(&p.name);
        let base = "/h/.local/state/balerix/plugins/web";
        let pool = "/h/.local/share/balerix/mise";
        let mise = Path::new("/opt/mise/bin/mise");
        let env = InstallEnv::default();
        let install = plugin_install_call(&p, &paths, &layout, mise)
            .unwrap()
            .profile(Path::new("/s/home"), &env);
        let trust = plugin_trust_call(&p, &paths, &layout, mise)
            .unwrap()
            .profile(Path::new("/s/home"), &env);
        let read = |extra: &[&str]| {
            let mut v = vec![
                "/usr",
                "/lib",
                "/lib64",
                "/bin",
                "/etc",
                "/opt/mise/bin/mise",
            ];
            v.extend(extra);
            json!(v)
        };
        assert_eq!(
            install["filesystem"],
            json!({
                "read": read(&["/data/plugins/web/abc123def456"]),
                "allow": [pool, format!("{base}/mise-state"), "/s/home"],
            })
        );
        assert_eq!(
            trust["filesystem"],
            json!({
                "read": read(&["/data/plugins/web/abc123def456", pool]),
                "allow": [format!("{base}/home"), "/s/home"],
            })
        );
        let shared = json!({
            "HOME": "/s/home",
            "TMPDIR": "/s/home/tmp",
            "MISE_GLOBAL_CONFIG_FILE": "/data/plugins/web/abc123def456/mise.toml",
            "MISE_DATA_DIR": pool,
            "MISE_CEILING_PATHS": format!("/data/plugins/web:{base}/home"),
            "MISE_YES": "1",
            "MISE_QUIET": "1",
            "MISE_AUTO_INSTALL": "false",
        });
        let with = |dirs: serde_json::Value| {
            let mut vars = shared.clone();
            for (k, v) in dirs.as_object().unwrap() {
                vars[k] = v.clone();
            }
            json!({ "allow_vars": crate::install_sandbox::PASSED_VARS, "set_vars": vars })
        };
        assert_eq!(
            install["environment"],
            with(json!({
                "MISE_CONFIG_DIR": format!("{base}/mise-state/config"),
                "MISE_STATE_DIR": format!("{base}/mise-state/state"),
                "MISE_CACHE_DIR": format!("{base}/mise-state/cache"),
            }))
        );
        assert_eq!(
            trust["environment"],
            with(json!({
                "MISE_CONFIG_DIR": format!("{base}/home/.config/mise"),
                "MISE_STATE_DIR": format!("{base}/home/.local/state/mise"),
                "MISE_CACHE_DIR": format!("{base}/home/.cache/mise"),
            }))
        );
        // the trust record lands where the plugin's own `mise run` looks
        let env = plugin_env(&p, &paths, &layout, "http://127.0.0.1:7643", "t").unwrap();
        assert_eq!(
            trust["environment"]["set_vars"]["MISE_STATE_DIR"],
            env["MISE_STATE_DIR"].as_str()
        );
        for profile in [&install, &trust] {
            assert_eq!(profile["security"], json!({ "signal_mode": "isolated" }));
            assert_eq!(profile["workdir"], json!({ "access": "none" }));
            assert!(profile.get("network").is_none());
        }
    }
}
