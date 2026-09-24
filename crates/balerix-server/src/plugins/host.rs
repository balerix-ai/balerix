//! The plugin host (plugins spec §5.2): one ordinary fleet actor for the
//! reserved `balerix` fleet, fed the synthetic spec from `plugins.yaml`,
//! with `hello` as its readiness event. The per-agent hook secret the actor
//! mints is the plugin's `BALERIX_PLUGIN_TOKEN`.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use balerix_api::{
    AgentPhase, CredentialBundle, FleetSpec, HelloRequest, HelloResponse, PLUGIN_PROTOCOL,
    PluginStatus, SpecHash, SyncReport,
};
use balerix_core::{
    AgentId, AgentName, Clock, FleetRecord, FleetSecrets, Materializer, RESERVED_FLEET,
    ResolvedPlugin, plugin_fleet, plugin_id,
};
use tokio::sync::{Mutex, oneshot, watch};

use super::PluginError;
use super::config::{load_plugins_file, resolve_secrets, resolve_source};
use super::manifest::read_manifest;
use super::materializer::{NullStore, PluginMaterializer};
use super::package;
use super::registry::PluginRegistry;
use crate::actor::{self, FleetHandle, Msg, Ports, READY_EVENT, Shared};
use crate::daemon::DaemonError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginHostConfig {
    /// `$XDG_CONFIG_HOME/balerix/plugins.yaml`.
    pub plugins_file: PathBuf,
    /// `$XDG_DATA_HOME/balerix/plugins`: unpacked packages.
    pub install_root: PathBuf,
}

/// How long `purge` waits for the actor to finish stopping the plugin
/// before refusing to delete its state.
const PURGE_WAIT: Duration = Duration::from_secs(30);

/// Waits until `id` is absent from the published record. `Err(())` on
/// timeout; a closed channel means the actor is gone and nothing of it is
/// running, which is `Ok`.
async fn wait_until_stopped(
    status: &mut watch::Receiver<FleetRecord>,
    id: &str,
    timeout: Duration,
) -> Result<(), ()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !status.borrow_and_update().status.agents.contains_key(id) {
            return Ok(());
        }
        match tokio::time::timeout_at(deadline, status.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Ok(()),
            Err(_) => return Err(()),
        }
    }
}

pub struct PluginHost {
    config: PluginHostConfig,
    materializer: Arc<PluginMaterializer>,
    handle: FleetHandle,
    clock: Arc<dyn Clock>,
    /// Where each plugin listens, whether it is ready, and its activation
    /// rows: the daemon and the host share one registry.
    registry: Arc<PluginRegistry>,
    /// One sync at a time; a second `plugin sync` waits.
    syncing: Mutex<()>,
}

impl PluginHost {
    /// Spawns the `balerix` fleet's actor with an empty spec. Nothing runs
    /// until the first `sync`.
    pub fn start(
        config: PluginHostConfig,
        agent_ports: &Ports,
        shared: Shared,
        registry: Arc<PluginRegistry>,
    ) -> Arc<Self> {
        let materializer = Arc::new(PluginMaterializer::new(agent_ports.materializer.clone()));
        let ports = Arc::new(Ports {
            materializer: materializer.clone(),
            runner: agent_ports.runner.clone(),
            clock: agent_ports.clock.clone(),
            store: Arc::new(NullStore),
            workspace: agent_ports.workspace.clone(),
            resolver: agent_ports.resolver.clone(),
            credentials: agent_ports.credentials.clone(),
            policy: agent_ports.policy.clone(),
            hook_url: agent_ports.hook_url.clone(),
            resync: agent_ports.resync,
        });
        let name = RESERVED_FLEET.parse().unwrap_or_else(|_| unreachable!());
        let record = FleetRecord::new(plugin_fleet(&[]).into());
        let handle = actor::spawn(name, record, FleetSecrets::default(), ports, shared, false);
        // The way down: a plugin that exits, restarts or is removed stops
        // being called. Only *transitions* count — a record the actor
        // publishes can still predate a `hello` that already marked the
        // plugin ready (the `hello` event reaches the actor behind the
        // pass), and taking readiness back on one of those would silence
        // a plugin that is up.
        let mut rx = handle.status.clone();
        let reg = registry.clone();
        tokio::spawn(async move {
            let mut ready: BTreeSet<AgentName> = BTreeSet::new();
            loop {
                {
                    let record = rx.borrow_and_update();
                    let mut seen = BTreeSet::new();
                    for (id, st) in &record.status.agents {
                        let Ok(id) = id.parse::<AgentId>() else {
                            continue;
                        };
                        seen.insert(id.agent.clone());
                        if st.phase == AgentPhase::Ready {
                            reg.set_ready(&id.agent, true);
                            ready.insert(id.agent);
                        } else if ready.remove(&id.agent) {
                            reg.set_ready(&id.agent, false);
                        }
                    }
                    // A plugin the record no longer holds is gone too.
                    let gone: Vec<AgentName> = ready.difference(&seen).cloned().collect();
                    for name in gone {
                        ready.remove(&name);
                        reg.set_ready(&name, false);
                    }
                }
                if rx.changed().await.is_err() {
                    return;
                }
            }
        });
        Arc::new(Self {
            config,
            materializer,
            handle,
            clock: agent_ports.clock.clone(),
            registry,
            syncing: Mutex::new(()),
        })
    }

    pub fn handle(&self) -> &FleetHandle {
        &self.handle
    }

    pub fn record(&self) -> FleetRecord {
        self.handle.status.borrow().clone()
    }

    /// Reads `plugins.yaml`, installs or locates every package, validates
    /// every manifest. Blocking; the caller runs it in `spawn_blocking`.
    /// Every error carries the entry's path so the operator knows which
    /// plugin is wrong.
    pub fn resolve(config: &PluginHostConfig) -> Result<Vec<ResolvedPlugin>, PluginError> {
        let file = load_plugins_file(&config.plugins_file)?;
        let base = config
            .plugins_file
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let entry_error = |i: usize, field: &str, message: String| PluginError::Config {
            path: if field.is_empty() {
                format!("plugins[{i}]")
            } else {
                format!("plugins[{i}].{field}")
            },
            message,
        };
        let mut out = Vec::new();
        for (i, entry) in file.plugins.iter().enumerate() {
            let source = resolve_source(entry, &base)
                .map_err(|e| entry_error(i, "source", e.to_string()))?;
            let (dir, digest) = package::install(
                &entry.name,
                &source,
                entry.sha256.as_deref(),
                &config.install_root,
            )
            .map_err(|e| match e {
                PluginError::Digest { expected, got } => entry_error(
                    i,
                    "sha256",
                    format!("mismatch (expected {expected}, got {got})"),
                ),
                other => entry_error(i, "source", other.to_string()),
            })?;
            let manifest = read_manifest(&dir).map_err(|e| entry_error(i, "", e.to_string()))?;
            if manifest.name != entry.name {
                return Err(entry_error(
                    i,
                    "name",
                    format!("manifest says {:?}", manifest.name),
                ));
            }
            let name: AgentName = entry
                .name
                .parse()
                .map_err(|e: balerix_core::NameError| entry_error(i, "name", e.to_string()))?;
            out.push(ResolvedPlugin {
                name,
                package: dir,
                manifest,
                config: resolve_secrets(entry, &base).map_err(|e| match e {
                    PluginError::Config { path, message } => entry_error(i, &path, message),
                    other => other,
                })?,
                digest,
            });
        }
        Ok(out)
    }

    /// Reconciles the running set to `plugins.yaml`: resolves, swaps the
    /// materializer's map, applies the synthetic spec. The actor's pass
    /// then stops removed plugins, restarts changed ones and starts new
    /// ones. Nothing changes when `resolve` fails.
    pub async fn sync(&self) -> Result<SyncReport, PluginError> {
        let _guard = self.syncing.lock().await;
        let cfg = self.config.clone();
        let resolved = tokio::task::spawn_blocking(move || Self::resolve(&cfg))
            .await
            .map_err(|e| PluginError::Internal(format!("sync task panicked: {e}")))??;
        let before: BTreeMap<AgentName, SpecHash> = self
            .materializer
            .all()
            .iter()
            .map(|p| (p.name.clone(), p.hash()))
            .collect();
        let mut report = SyncReport::default();
        for p in &resolved {
            match before.get(&p.name) {
                Some(h) if *h == p.hash() => report.unchanged.push(p.name.to_string()),
                _ => report.installed.push(p.name.to_string()),
            }
        }
        for name in before.keys() {
            if !resolved.iter().any(|p| &p.name == name) {
                report.stopped.push(name.to_string());
            }
        }
        self.materializer.replace(resolved.clone());
        self.registry.replace_plugins(&resolved, &report.unchanged);
        let spec: FleetSpec = plugin_fleet(&resolved).into();
        let (reply, rx) = oneshot::channel();
        self.handle
            .tx
            .send(Msg::Apply {
                spec,
                credentials: CredentialBundle::default(),
                reply,
            })
            .await
            .map_err(|_| PluginError::Internal("plugin actor is gone".into()))?;
        rx.await
            .map_err(|_| PluginError::Internal("plugin actor dropped the request".into()))?;
        Ok(report)
    }

    /// The plugin is up: record where it listens, hand back its config,
    /// and tell the actor — `hello` is the plugin's `SessionStart`.
    pub async fn hello(
        &self,
        name: &AgentName,
        req: HelloRequest,
        token: &str,
    ) -> Result<HelloResponse, DaemonError> {
        let plugin = self
            .materializer
            .get(name)
            .ok_or(DaemonError::Unauthorized)?;
        if req.name != name.as_str() {
            return Err(DaemonError::Invalid(format!(
                "hello.name: {:?} does not match the token's plugin {:?}",
                req.name,
                name.as_str()
            )));
        }
        if req.protocol != PLUGIN_PROTOCOL {
            return Err(DaemonError::Invalid(format!(
                "hello.protocol: this daemon speaks protocol {PLUGIN_PROTOCOL}, got {}",
                req.protocol
            )));
        }
        let addr: SocketAddr = req.listen.parse().map_err(|_| {
            DaemonError::Invalid(format!("hello.listen: {:?} is not host:port", req.listen))
        })?;
        if !addr.ip().is_loopback() {
            return Err(DaemonError::Invalid(
                "hello.listen: must be a loopback address".into(),
            ));
        }
        self.registry
            .set_listen(name, req.listen.clone(), token.to_string());
        self.handle
            .tx
            .send(Msg::Event {
                agent: plugin.id(),
                name: READY_EVENT.into(),
                at: self.clock.now(),
            })
            .await
            .map_err(|_| DaemonError::Internal("plugin actor is gone".into()))?;
        Ok(HelloResponse {
            config: plugin.config.clone(),
        })
    }

    /// One row per declared plugin, sorted by name.
    pub async fn list(&self) -> Vec<PluginStatus> {
        let record = self.record();
        self.materializer
            .all()
            .into_iter()
            .map(|p| {
                let st = record.status.agents.get(&p.id().to_string());
                let info = self.registry.plugin(&p.name);
                // The actor's own message wins; otherwise the health
                // poller's verdict, if any (§16.5).
                let message = match st.map(|s| s.message.clone()).filter(|m| !m.is_empty()) {
                    Some(m) => m,
                    None => info
                        .as_ref()
                        .and_then(|i| i.degraded.as_ref())
                        .map(|d| format!("degraded: {d}"))
                        .unwrap_or_default(),
                };
                PluginStatus {
                    name: p.name.to_string(),
                    version: p.manifest.version.clone(),
                    phase: st.map_or(AgentPhase::Pending, |s| s.phase),
                    listen: info.as_ref().and_then(|i| i.listen.clone()),
                    routes: p.manifest.routes,
                    active_agents: self.registry.active_agents(&p.name),
                    message,
                }
            })
            .collect()
    }

    /// `plugin remove --purge`: only for a plugin no longer declared.
    /// Deletes `plugins/<name>/` through the materializer and the installed
    /// packages under `install_root/<name>/`.
    ///
    /// Holds the sync lock and waits for the actor to have taken the agent
    /// out of the record before deleting: the actor answers `Apply` before
    /// running the pass that stops the plugin, so a `sync` + `purge` pair
    /// would otherwise delete `plugins/<name>/` out from under a process
    /// that is still running.
    pub async fn purge(&self, name: &AgentName) -> Result<(), PluginError> {
        let _guard = self.syncing.lock().await;
        if self.materializer.get(name).is_some() {
            return Err(PluginError::StillDeclared(name.to_string()));
        }
        wait_until_stopped(
            &mut self.handle.status.clone(),
            &plugin_id(name).to_string(),
            PURGE_WAIT,
        )
        .await
        .map_err(|()| PluginError::Internal(format!("plugin {name} is still stopping")))?;
        let materializer = self.materializer.clone();
        let packages = self.config.install_root.join(name.as_str());
        let name = name.clone();
        tokio::task::spawn_blocking(move || {
            materializer
                .purge_plugin(&name)
                .map_err(|e| PluginError::Internal(e.to_string()))?;
            match std::fs::remove_dir_all(&packages) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(PluginError::io(&packages, e)),
            }
        })
        .await
        .map_err(|e| PluginError::Internal(format!("purge task panicked: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_with(agent: Option<&str>) -> FleetRecord {
        let mut record = FleetRecord::new(plugin_fleet(&[]).into());
        if let Some(id) = agent {
            record.status.entry(id);
        }
        record
    }

    /// `purge` may only delete once the actor's pass has taken the agent
    /// out of the record — the `Apply` reply lands before that pass.
    #[tokio::test]
    async fn the_purge_wait_returns_when_the_agent_leaves_the_record() {
        let id = "balerix/plugins/hello";
        let (tx, rx) = watch::channel(record_with(Some(id)));
        let mut rx2 = rx.clone();
        let waiter =
            tokio::spawn(async move { wait_until_stopped(&mut rx2, id, PURGE_WAIT).await });
        // still there: the wait is pending
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        tx.send_replace(record_with(Some(id)));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished(), "an unrelated update is not enough");
        tx.send_replace(record_with(None));
        assert_eq!(waiter.await.unwrap_or_else(|e| panic!("{e}")), Ok(()));

        // an actor that is gone entirely is not something to wait for
        let (tx, mut rx) = watch::channel(record_with(Some(id)));
        drop(tx);
        assert_eq!(wait_until_stopped(&mut rx, id, PURGE_WAIT).await, Ok(()));
    }

    #[tokio::test]
    async fn the_purge_wait_times_out_while_the_agent_is_still_there() {
        let id = "balerix/plugins/hello";
        let (_tx, mut rx) = watch::channel(record_with(Some(id)));
        let start = std::time::Instant::now();
        assert_eq!(
            wait_until_stopped(&mut rx, id, Duration::from_millis(50)).await,
            Err(())
        );
        assert!(start.elapsed() >= Duration::from_millis(50));
        // a record that never held it does not wait at all
        let (_tx, mut rx) = watch::channel(record_with(None));
        assert_eq!(
            wait_until_stopped(&mut rx, id, Duration::from_millis(50)).await,
            Ok(())
        );
    }
}
