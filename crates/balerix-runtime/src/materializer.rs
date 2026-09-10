//! `Runtime`: the `Materializer` over real tools. `render_agent` is the
//! pure-ish half (files only) shared with `balerix dev materialize`.

use std::time::Duration;

use balerix_api::{CredentialBundle, GitAuth, GitSettings};
use balerix_core::{
    AgentId, AgentName, CrewRef, CrewTools, HookTarget, Keep, LaunchPlan, MaterializeError,
    Materializer, RepoRef, ResolvedAgent, ResolvedPlugin, SystemToolchain,
};

use crate::env::agent_env;
use crate::home::{HomeInputs, write_home};
use crate::launch::{hooks_port, render_launch, wants_continue, write_launch};
use crate::layout::StateLayout;
use crate::sandbox::{balerix_grants, render_profile, validate_profile, write_profile};
use crate::toolchain::{Toolchain, system_tools};
use crate::tools::ToolPaths;
use crate::workspace::Workspace;

pub struct Runtime {
    pub layout: StateLayout,
    pub tools: ToolPaths,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions {
    pub redact_credentials: bool,
}

/// What `render_agent` produced and whether the installable inputs moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOutcome {
    pub plan: LaunchPlan,
    /// `mise.toml` or `nono-profile.json` changed: the install marker was
    /// removed and `install_and_validate` will run the tools again.
    pub toolchain_changed: bool,
}

impl Runtime {
    pub fn new(layout: StateLayout, tools: ToolPaths) -> Self {
        Self { layout, tools }
    }

    /// Steps 2–5 of the pipeline without subprocesses: home, `mise.toml`,
    /// `nono-profile.json`, `launch.sh`. Idempotent.
    pub fn render_agent(
        &self,
        agent: &ResolvedAgent,
        creds: &CredentialBundle,
        hooks: &HookTarget,
        opts: &RenderOptions,
    ) -> Result<RenderOutcome, MaterializeError> {
        let id = &agent.id;
        let paths = self.layout.agent(id);
        let crew = self.layout.crew(&id.crew_ref());
        let with_gh = agent.git.auth == GitAuth::Gh;

        write_home(
            id,
            &paths,
            &HomeInputs {
                settings: &agent.settings.claude.settings,
                creds,
                hooks,
                git: &agent.git,
                relay: &self.tools.balerix,
                trusted: &[&paths.workspace, &crew.repo],
                with_gh,
                redact_credentials: opts.redact_credentials,
            },
        )?;

        let system = system_tools(&self.layout, &id.to_string())?;
        let tools_changed = Toolchain {
            tools: &self.tools,
            layout: &self.layout,
        }
        .write(id, &paths, &system, &agent.settings.tools, with_gh)?;

        let env = agent_env(
            id,
            &paths,
            &self.layout,
            &hooks.url,
            &hooks.secret,
            &agent.settings.env,
        );
        let profile = render_profile(
            id,
            &balerix_grants(
                id,
                &paths,
                &crew,
                &self.layout,
                &self.tools.balerix,
                &self.tools.mise,
            ),
            hooks_port(&hooks.url),
            &env,
            &agent.settings.sandbox,
        )?;
        let profile_changed = write_profile(id, &paths, &profile)?;

        // Removed as soon as either rendered file has changed (Phase 3 spec
        // §6.2), before any later fallible step (e.g. `write_launch`) can
        // return early and strand a marker whose files no longer match what
        // was last installed and validated.
        let toolchain_changed = tools_changed || profile_changed;
        if toolchain_changed
            && let Err(e) = std::fs::remove_file(paths.installed_marker())
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(MaterializeError::Io {
                id: id.to_string(),
                path: paths.installed_marker(),
                message: e.to_string(),
            });
        }

        let resume = wants_continue(&agent.settings.claude, &paths);
        let (script, plan) = render_launch(
            &paths,
            &self.tools,
            &agent.settings.claude.binary,
            &agent.settings.claude.args,
            resume,
        );
        write_launch(id, &paths, &script)?;

        Ok(RenderOutcome {
            plan,
            toolchain_changed,
        })
    }

    /// The subprocess half of steps 3 and 4. Skipped when the marker from a
    /// previous success exists (Phase 3 spec §6.2); written on success.
    pub fn install_and_validate(&self, agent: &ResolvedAgent) -> Result<(), MaterializeError> {
        let paths = self.layout.agent(&agent.id);
        if paths.installed_marker().exists() {
            return Ok(());
        }
        Toolchain {
            tools: &self.tools,
            layout: &self.layout,
        }
        .install(&agent.id, &paths)?;
        validate_profile(&self.tools, &agent.id, &paths)?;
        std::fs::write(paths.installed_marker(), b"").map_err(|e| MaterializeError::Io {
            id: agent.id.to_string(),
            path: paths.installed_marker(),
            message: e.to_string(),
        })
    }

    /// The two fleet-owned pools, outermost first: the fleet's tools into
    /// the fleet pool, this crew's into the crew pool. Each is skipped when
    /// its marker already matches its rendered table, and each resolves
    /// through the pools above it, so a version an outer pool already holds
    /// is never downloaded twice (Spec E §5). The daemon pool is not
    /// installed here: it belongs to the `SystemPool` actor (Spec F §3).
    pub fn install_pools(
        &self,
        crew: &CrewRef,
        tools: CrewTools<'_>,
    ) -> Result<(), MaterializeError> {
        let tc = Toolchain {
            tools: &self.tools,
            layout: &self.layout,
        };
        let fleet = self.layout.fleet(&crew.fleet);
        let crew_paths = self.layout.crew(crew);
        let daemon_pool = self.layout.mise_data_dir();
        let log = crew_paths.root.join("logs").join("mise.pools.log");

        let crew_id = crew.to_string();
        tc.install_level(
            &crew_id,
            &format!("fleet {}", crew.fleet),
            &fleet.mise_toml,
            &fleet.mise_pool(),
            std::slice::from_ref(&daemon_pool),
            &fleet.installed_marker(),
            tools.fleet,
            &log,
            None,
        )?;
        tc.install_level(
            &crew_id,
            &format!("crew {crew}"),
            &crew_paths.mise_toml(),
            &crew_paths.mise_pool(),
            &[fleet.mise_pool(), daemon_pool],
            &crew_paths.installed_marker(),
            tools.crew,
            &log,
            None,
        )
    }
}

impl SystemToolchain for Runtime {
    /// The daemon pool, owned by the `SystemPool` actor (Spec F §3). Returns
    /// `Ok` only when the pool matches the system table *and* exists: a
    /// marker that survived a deleted pool would otherwise report ready over
    /// an empty directory (F-5).
    fn ensure_system_pool(&self) -> Result<(), MaterializeError> {
        let tc = Toolchain {
            tools: &self.tools,
            layout: &self.layout,
        };
        let pool = self.layout.mise_data_dir();
        let marker = self.layout.system_installed_marker();
        let log = self
            .layout
            .server_dir()
            .join("logs")
            .join("mise.system.log");
        let system = system_tools(&self.layout, "system")?;
        // Drop a stale marker so `install_level` cannot short-circuit past a
        // pool that is no longer there.
        if !pool.exists()
            && let Err(e) = std::fs::remove_file(&marker)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(MaterializeError::Io {
                id: "system".to_string(),
                path: marker,
                message: e.to_string(),
            });
        }
        tc.install_level(
            "system",
            "system",
            &self.layout.system_mise_toml_generated(),
            &pool,
            &[],
            &marker,
            &system,
            &log,
            Some(SYSTEM_POOL_INSTALL_TIMEOUT),
        )
    }
}

/// How long `ensure_system_pool` lets one `mise trust`/`mise install` call
/// run before killing it (Spec F §6). 600s is the value Spec F's `SystemPool`
/// actor design documents as its default `attempt_timeout` (Task 4); once
/// the actor's configured value is threaded down here instead, this constant
/// goes away.
const SYSTEM_POOL_INSTALL_TIMEOUT: Duration = Duration::from_secs(600);

impl Runtime {
    fn workspace(&self, fleet: &balerix_core::FleetName, git: &GitSettings) -> Workspace<'_> {
        Workspace {
            tools: &self.tools,
            gh_config_dir: (git.auth == GitAuth::Gh).then(|| self.layout.fleet_gh_dir(fleet)),
        }
    }

    fn rm_rf(id: &str, path: &std::path::Path) -> Result<(), MaterializeError> {
        retry_rmdir(path, RM_RF_WINDOW, RM_RF_STEP, |p| {
            std::fs::remove_dir_all(p)
        })
        .map_err(|e| MaterializeError::Io {
            id: id.to_string(),
            path: path.to_path_buf(),
            message: e.to_string(),
        })
    }
}

/// How long `rm_rf` keeps retrying a directory that refills under it, and
/// how long it waits between attempts.
const RM_RF_WINDOW: Duration = Duration::from_secs(5);
const RM_RF_STEP: Duration = Duration::from_millis(100);

/// `remove_dir_all` with a bounded retry on `DirectoryNotEmpty`: nono
/// flushes its audit ledger and session file under `<root>/nono/` shortly
/// after `tmux kill-window` returns, so a tree that was quiet when the
/// walk started can refill under it. `NotFound` is success; every other
/// error is returned at once, since only a concurrent writer is worth
/// waiting for.
pub(crate) fn retry_rmdir(
    path: &std::path::Path,
    window: Duration,
    step: Duration,
    mut remove: impl FnMut(&std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + window;
    loop {
        match remove(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                if e.kind() != std::io::ErrorKind::DirectoryNotEmpty
                    || std::time::Instant::now() >= deadline
                {
                    return Err(e);
                }
                std::thread::sleep(step);
            }
        }
    }
}

impl Materializer for Runtime {
    fn ensure_crew(
        &self,
        crew: &CrewRef,
        repo: &RepoRef,
        git_ref: &str,
        git: &GitSettings,
        creds: &CredentialBundle,
        tools: CrewTools<'_>,
    ) -> Result<(), MaterializeError> {
        let id = crew.to_string();
        if git.auth == GitAuth::Gh {
            let token = creds.gh_token.as_deref().ok_or_else(|| MaterializeError::Invalid {
                id: id.clone(),
                message: "git.auth is gh but no gh token was provided (run `gh auth login` on the client)".into(),
            })?;
            Workspace::write_fleet_gh_config(&self.layout.fleet_gh_dir(&crew.fleet), token, &id)?;
        }
        self.install_pools(crew, tools)?;
        self.workspace(&crew.fleet, git)
            .ensure_repo(&id, &self.layout.crew(crew), repo, git_ref)
    }

    fn materialize(
        &self,
        agent: &ResolvedAgent,
        creds: &CredentialBundle,
        hooks: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        let id = agent.id.to_string();
        let crew = self.layout.crew(&agent.id.crew_ref());
        let paths = self.layout.agent(&agent.id);
        self.workspace(&agent.id.fleet, &agent.git)
            .ensure_worktree(
                &id,
                &crew,
                &paths.workspace,
                &agent.branch(),
                &agent.git_ref,
            )?;
        let out = self.render_agent(agent, creds, hooks, &RenderOptions::default())?;
        self.install_and_validate(agent)?;
        Ok(out.plan)
    }

    fn remove_agent(&self, agent: &AgentId) -> Result<(), MaterializeError> {
        let id = agent.to_string();
        let crew = self.layout.crew(&agent.crew_ref());
        let paths = self.layout.agent(agent);
        Workspace {
            tools: &self.tools,
            gh_config_dir: None,
        }
        .remove_worktree(&id, &crew, &paths.workspace)?;
        Self::rm_rf(&id, &paths.root)
    }

    fn remove_crew(&self, crew: &CrewRef, keep: Keep) -> Result<(), MaterializeError> {
        let id = crew.to_string();
        let paths = self.layout.crew(crew);
        if !keep.sessions {
            let agents_dir = paths.root.join("agents");
            if let Ok(entries) = std::fs::read_dir(&agents_dir) {
                for e in entries.flatten() {
                    let agent_id = format!("{id}/{}", e.file_name().to_string_lossy());
                    Workspace {
                        tools: &self.tools,
                        gh_config_dir: None,
                    }
                    .remove_worktree(
                        &agent_id,
                        &paths,
                        &e.path().join("workspace"),
                    )?;
                }
            }
            Self::rm_rf(&id, &agents_dir)?;
        }
        if !keep.repos {
            Self::rm_rf(&id, &paths.repo)?;
        }
        if !keep.repos && !keep.sessions {
            Self::rm_rf(&id, &paths.root)?;
        }
        Ok(())
    }

    fn materialize_plugin(
        &self,
        plugin: &ResolvedPlugin,
        host: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        let out = self.render_plugin(plugin, host)?;
        self.install_plugin(plugin)?;
        Ok(out.plan)
    }

    fn purge_plugin(&self, name: &AgentName) -> Result<(), MaterializeError> {
        let root = self.layout.plugin(name).root;
        Self::rm_rf(&balerix_core::plugin_id(name).to_string(), &root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{AgentSettings, CrewSpec, FleetSpec};
    use balerix_core::Fleet;
    use std::collections::BTreeMap;

    fn runtime(root: &std::path::Path) -> Runtime {
        let layout = StateLayout {
            state_root: root.join("state"),
            data_root: root.join("data"),
            config_root: root.join("config"),
        };
        std::fs::create_dir_all(&layout.config_root).unwrap();
        std::fs::write(layout.system_mise_toml(), "[tools]\n").unwrap();
        let tools = ToolPaths {
            git: "/nonexistent/git".into(),
            gh: "/nonexistent/gh".into(),
            mise: "/nonexistent/mise".into(),
            nono: "/nonexistent/nono".into(),
            tmux: "/nonexistent/tmux".into(),
            balerix: "/nonexistent/balerix".into(),
        };
        Runtime::new(layout, tools)
    }

    fn agent(tools: &[(&str, &str)]) -> ResolvedAgent {
        let s = AgentSettings {
            tools: tools
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..AgentSettings::default()
        };
        let fleet = Fleet::try_from(FleetSpec {
            name: "f".into(),
            crews: BTreeMap::from([(
                "c".to_string(),
                CrewSpec {
                    repo: "acme/x".into(),
                    git_ref: "main".into(),
                    git: GitSettings::default(),
                    agents: BTreeMap::from([("a".to_string(), s)]),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        })
        .unwrap();
        ResolvedAgent::from_fleet(&fleet).remove(0)
    }

    fn hooks() -> HookTarget {
        HookTarget {
            url: "http://127.0.0.1:7643".into(),
            secret: "s".into(),
        }
    }

    #[test]
    fn rendering_reports_toolchain_changes_and_clears_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path());
        let creds = CredentialBundle::default();
        let a = agent(&[("node", "22.11.0")]);
        let paths = rt.layout.agent(&a.id);
        let first = rt
            .render_agent(&a, &creds, &hooks(), &RenderOptions::default())
            .unwrap();
        assert!(first.toolchain_changed);
        let again = rt
            .render_agent(&a, &creds, &hooks(), &RenderOptions::default())
            .unwrap();
        assert!(!again.toolchain_changed, "same inputs, same files");
        assert_eq!(again.plan, first.plan);

        std::fs::write(paths.installed_marker(), "").unwrap();
        rt.render_agent(&a, &creds, &hooks(), &RenderOptions::default())
            .unwrap();
        assert!(
            paths.installed_marker().exists(),
            "unchanged render keeps the marker"
        );
        let b = agent(&[("node", "22.12.0")]);
        let changed = rt
            .render_agent(&b, &creds, &hooks(), &RenderOptions::default())
            .unwrap();
        assert!(changed.toolchain_changed);
        assert!(
            !paths.installed_marker().exists(),
            "a new table invalidates the install"
        );
    }

    #[test]
    fn install_is_skipped_when_the_marker_exists() {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path());
        let a = agent(&[]);
        let paths = rt.layout.agent(&a.id);
        rt.render_agent(
            &a,
            &CredentialBundle::default(),
            &hooks(),
            &RenderOptions::default(),
        )
        .unwrap();
        // tools point nowhere: running mise would fail with "cannot execute"
        assert!(rt.install_and_validate(&a).is_err());
        std::fs::write(paths.installed_marker(), "").unwrap();
        rt.install_and_validate(&a).unwrap();
    }

    #[test]
    fn a_matching_marker_with_no_pool_is_treated_as_stale() {
        use sha2::Digest;

        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path());
        let layout = &rt.layout;

        // Write a marker holding the digest of the table that would be
        // rendered, but never create the pool.
        let system = system_tools(layout, "system").unwrap();
        let text = crate::toolchain::render_level_toml("system", &system);
        let digest = hex::encode(sha2::Sha256::digest(text.as_bytes()));
        std::fs::create_dir_all(layout.system_installed_marker().parent().unwrap()).unwrap();
        std::fs::write(layout.system_installed_marker(), &digest).unwrap();
        assert!(!layout.mise_data_dir().exists(), "no pool yet");

        // With a fake mise that cannot really install, the call must still
        // *attempt* it rather than short-circuit on the marker.
        let err = rt.ensure_system_pool().unwrap_err();
        assert!(
            !matches!(err, MaterializeError::Invalid { .. }),
            "a missing pool must drive a real install attempt, got {err:?}"
        );
    }

    #[test]
    fn a_marker_removal_failure_other_than_missing_is_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path());
        let marker = rt.layout.system_installed_marker();
        // A directory where a file is expected: `remove_file` fails with
        // something other than `NotFound`, and the pool does not exist
        // either, so `ensure_system_pool` must try the removal and
        // propagate that failure rather than silently proceed to
        // `install_level` with a marker that is still there.
        std::fs::create_dir_all(&marker).unwrap();
        assert!(!rt.layout.mise_data_dir().exists(), "no pool yet");

        let err = rt.ensure_system_pool().unwrap_err();
        assert!(
            matches!(err, MaterializeError::Io { .. }),
            "a removal failure other than NotFound must surface as Io, got {err:?}"
        );
    }

    #[test]
    fn a_later_failure_does_not_strand_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path());
        let creds = CredentialBundle::default();
        let a = agent(&[("node", "22.11.0")]);
        let paths = rt.layout.agent(&a.id);
        rt.render_agent(&a, &creds, &hooks(), &RenderOptions::default())
            .unwrap();
        std::fs::write(paths.installed_marker(), "").unwrap();
        assert!(paths.installed_marker().exists());

        // A changed toolchain (so render_agent must invalidate the marker),
        // but `launch.sh`'s path is now a directory: `write_launch` (which
        // runs after the marker check) fails, and render_agent must return
        // that error early. The marker must still be gone — it is removed
        // as soon as `tools_changed`/`profile_changed` is known, not after
        // this later, unrelated fallible step.
        let b = agent(&[("node", "22.12.0")]);
        std::fs::remove_file(&paths.launch).unwrap();
        std::fs::create_dir(&paths.launch).unwrap();
        let err = rt
            .render_agent(&b, &creds, &hooks(), &RenderOptions::default())
            .unwrap_err();
        assert!(matches!(err, MaterializeError::Io { .. }), "{err:?}");
        assert!(
            !paths.installed_marker().exists(),
            "marker must be removed even though a later step failed"
        );
    }

    fn enotempty() -> std::io::Error {
        std::io::Error::from(std::io::ErrorKind::DirectoryNotEmpty)
    }

    /// nono keeps writing under `plugins/<name>/nono/` for a moment after
    /// `tmux kill-window` returns, so a `remove_dir_all` that loses that
    /// race must be retried rather than reported (`purge` and `down
    /// --purge` both go through `rm_rf`).
    #[test]
    fn rm_rf_retries_a_directory_that_refills_under_it() {
        let path = std::path::Path::new("/does/not/matter");
        let mut calls = 0;
        let r = retry_rmdir(path, Duration::from_secs(5), Duration::ZERO, |_| {
            calls += 1;
            if calls < 3 { Err(enotempty()) } else { Ok(()) }
        });
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(calls, 3, "retried until the writer was done");

        // a window that runs out returns the last error, and only after
        // the window has actually elapsed
        let mut calls = 0;
        let start = std::time::Instant::now();
        let e = retry_rmdir(
            path,
            Duration::from_millis(50),
            Duration::from_millis(10),
            |_| {
                calls += 1;
                Err(enotempty())
            },
        )
        .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::DirectoryNotEmpty);
        assert!(start.elapsed() >= Duration::from_millis(50));
        assert!(calls > 1, "retried {calls} times");

        // NotFound is success; any other error is returned at once
        let mut calls = 0;
        assert!(
            retry_rmdir(path, Duration::from_secs(5), Duration::ZERO, |_| {
                calls += 1;
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            })
            .is_ok()
        );
        assert_eq!(calls, 1);
        let mut calls = 0;
        let e = retry_rmdir(path, Duration::from_secs(5), Duration::ZERO, |_| {
            calls += 1;
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        })
        .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(calls, 1, "a permanent error is not waited out");
    }

    #[test]
    fn rm_rf_removes_a_tree_and_ignores_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("plugins/hello");
        std::fs::create_dir_all(root.join("nono")).unwrap();
        std::fs::write(root.join("nono/ledger"), "x").unwrap();
        Runtime::rm_rf("balerix/plugins/hello", &root).unwrap();
        assert!(!root.exists());
        Runtime::rm_rf("balerix/plugins/hello", &root).unwrap();
    }
}
