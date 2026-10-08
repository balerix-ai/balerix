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
use super::config::{Source, load_plugins_file, resolve_secrets, resolve_source};
use super::manifest::read_manifest;
use super::materializer::{NullStore, PluginMaterializer};
use super::package;
use super::registry::PluginRegistry;
use crate::actor::{self, FleetHandle, Msg, Ports, Shared};
use crate::daemon::DaemonError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginHostConfig {
    /// `$XDG_CONFIG_HOME/balerix/plugins.yaml`.
    pub plugins_file: PathBuf,
    /// `$XDG_DATA_HOME/balerix/plugins`: unpacked packages.
    pub install_root: PathBuf,
}

/// How long `purge` waits for the actor to run a pass and finish stopping
/// the plugin before refusing to delete its state. Below the CLI's 30 s
/// request timeout, so a purge that runs out answers its own error (#1).
pub const PURGE_WAIT: Duration = Duration::from_secs(20);

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

/// The way down: a plugin that exits, restarts or is removed stops being
/// called.
///
/// `hello` sets `ready` synchronously and only then tells the actor, so a
/// record can predate a `hello` that already marked the plugin ready, and
/// taking readiness back on one of those would silence a plugin that is
/// up. `watch` also coalesces publishes, so a `Ready` record and the death
/// after it can arrive as one observation of the death (#12). Neither
/// depends on what this loop saw before: the actor stamps each agent's
/// status with the generation of the newest `hello` it has taken
/// (`Msg::PluginHello`, in the same publish that makes it `Ready`), and a
/// record whose phase is anything but `Ready` takes readiness back only
/// when that stamp is at least the registry's current generation — i.e.
/// the record is newer than the plugin's last `hello`. A plugin the record
/// no longer holds is judged by the stamp it last carried here.
async fn mirror_readiness(mut rx: watch::Receiver<FleetRecord>, reg: Arc<PluginRegistry>) {
    let mut stamps: BTreeMap<AgentName, u64> = BTreeMap::new();
    loop {
        {
            let record = rx.borrow_and_update();
            let mut seen = BTreeSet::new();
            for (id, st) in &record.status.agents {
                let Ok(id) = id.parse::<AgentId>() else {
                    continue;
                };
                seen.insert(id.agent.clone());
                stamps.insert(id.agent.clone(), st.plugin_hello);
                if st.phase == AgentPhase::Ready {
                    reg.set_ready(&id.agent, true);
                } else {
                    reg.clear_ready_if(&id.agent, st.plugin_hello);
                }
            }
            stamps.retain(|name, stamp| {
                if seen.contains(name) {
                    return true;
                }
                reg.clear_ready_if(name, *stamp);
                false
            });
        }
        if rx.changed().await.is_err() {
            return;
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
            kube: agent_ports.kube.clone(),
        });
        let name = RESERVED_FLEET.parse().unwrap_or_else(|_| unreachable!());
        let record = FleetRecord::new(plugin_fleet(&[]).into());
        let handle = actor::spawn(name, record, FleetSecrets::default(), ports, shared, false);
        tokio::spawn(mirror_readiness(handle.status.clone(), registry.clone()));
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
            // `MISE_CEILING_PATHS` is colon-separated, and the package's
            // parent is one of its entries (#4)
            if dir.as_os_str().as_encoded_bytes().contains(&b':') {
                return Err(entry_error(
                    i,
                    "source",
                    format!("{}: a package path must not contain ':'", dir.display()),
                ));
            }
            // A directory is used in place, so nothing names its content:
            // the bytes of its two files stand in, so an edited
            // `mise.toml` or manifest changes the plugin's hash and is
            // installed and trusted again on the next sync (#4).
            let digest = match (&source, digest) {
                (Source::Directory(_), None) => Some(
                    package::directory_digest(&dir)
                        .map_err(|e| entry_error(i, "source", e.to_string()))?,
                ),
                (_, d) => d,
            };
            let manifest = read_manifest(&dir).map_err(|e| entry_error(i, "", e.to_string()))?;
            if manifest.name != entry.name {
                return Err(entry_error(
                    i,
                    "name",
                    format!("manifest says {:?}", manifest.name),
                ));
            }
            if let Some(reason) = balerix_core::reserved_plugin_reason(&entry.name) {
                return Err(entry_error(i, "name", reason.to_string()));
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
                fleet_defaults: fleet_defaults_layer(&entry.fleet_defaults)
                    .map_err(|message| entry_error(i, "fleetDefaults", message))?,
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
                agent_tokens: Default::default(),
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
        // Unreachable over HTTP: `plugin_hello` takes `name` from
        // `req.name` itself (the token is then checked against that
        // plugin), and names are not normalised. It guards a direct
        // caller that passes a name and a request that disagree.
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
        let generation = self
            .registry
            .set_listen(name, req.listen.clone(), token.to_string());
        self.handle
            .tx
            .send(Msg::PluginHello {
                agent: plugin.id(),
                generation,
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
    /// Holds the sync lock and, within one `PURGE_WAIT`, waits for a pass
    /// the actor runs after this request (`Msg::Barrier`), then for the
    /// agent to be out of the record, before deleting. The actor answers
    /// `Apply` before running the pass that stops the plugin, so a sync
    /// then a purge would otherwise delete `plugins/<name>/` out from
    /// under a process that is still running; and a window the record
    /// never held (one that outlived a daemon restart) is only stopped by
    /// a pass, which the record alone cannot show (#1).
    pub async fn purge(&self, name: &AgentName) -> Result<(), PluginError> {
        let _guard = self.syncing.lock().await;
        if self.materializer.get(name).is_some() {
            return Err(PluginError::StillDeclared(name.to_string()));
        }
        let deadline = tokio::time::Instant::now() + PURGE_WAIT;
        let stopping = || PluginError::Internal(format!("plugin {name} is still stopping"));
        let gone = || PluginError::Internal("plugin actor is gone".into());
        let (reply, rx) = oneshot::channel();
        tokio::time::timeout_at(deadline, self.handle.tx.send(Msg::Barrier { reply }))
            .await
            .map_err(|_| stopping())?
            .map_err(|_| gone())?;
        let ran = tokio::time::timeout_at(deadline, rx)
            .await
            .map_err(|_| stopping())?
            .map_err(|_| gone())?;
        if !ran {
            return Err(PluginError::Unavailable(format!(
                "plugin {name}: the daemon's tool pool is not ready, so no reconcile pass ran and its window may still be running; nothing was deleted, try again once the pool is ready"
            )));
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        wait_until_stopped(
            &mut self.handle.status.clone(),
            &plugin_id(name).to_string(),
            left,
        )
        .await
        .map_err(|()| stopping())?;
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

/// An entry's `fleetDefaults` as the layer the resolver gets (Spec M
/// §12.1): null reads as no layer, and anything else must be a mapping
/// shaped like an agent settings block, so the operator's typo fails the
/// sync under the entry's path rather than an apply under the plugin's
/// file. Nulls inside are dropped before the shape check: in the layer
/// they mean "delete", which the typed shape cannot hold.
fn fleet_defaults_layer(layer: &serde_json::Value) -> Result<serde_json::Value, String> {
    if layer.is_null() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    if !layer.is_object() {
        return Err("expected a mapping".into());
    }
    serde_json::from_value::<balerix_api::AgentSettings>(without_nulls(layer))
        .map_err(|e| e.to_string())?;
    Ok(layer.clone())
}

fn without_nulls(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => serde_json::Value::Object(
            m.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), without_nulls(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "apiVersion: balerix/v1\nkind: Plugin\nname: hello\nversion: 0.1.0\nprotocol: 1\nstart: serve\n";

    /// `plugins.yaml` with one directory source at `package`, written.
    fn host_config(top: &std::path::Path, package: &std::path::Path) -> PluginHostConfig {
        std::fs::create_dir_all(package).unwrap();
        std::fs::write(package.join("balerix-plugin.yaml"), MANIFEST).unwrap();
        std::fs::write(package.join("mise.toml"), "[tasks.serve]\nrun = \"true\"\n").unwrap();
        let plugins_file = top.join("plugins.yaml");
        std::fs::write(
            &plugins_file,
            format!(
                "plugins:\n  - name: hello\n    source: \"{}\"\n",
                package.display()
            ),
        )
        .unwrap();
        PluginHostConfig {
            plugins_file,
            install_root: top.join("install"),
        }
    }

    /// #4: `MISE_CEILING_PATHS` is colon-separated.
    #[test]
    fn a_package_path_with_a_colon_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let config = host_config(dir.path(), &dir.path().join("a:b"));
        let e = PluginHost::resolve(&config).unwrap_err().to_string();
        assert!(
            e.starts_with("plugins.yaml: plugins[0].source: ")
                && e.ends_with("a:b: a package path must not contain ':'"),
            "{e}"
        );
    }

    /// #4: a directory source's two files are part of its hash, so an
    /// edit is a reinstall on the next sync.
    #[test]
    fn editing_a_directory_sources_files_changes_its_hash() {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("hello");
        let config = host_config(dir.path(), &package);
        let hash = || PluginHost::resolve(&config).unwrap()[0].hash();
        let first = PluginHost::resolve(&config).unwrap();
        assert!(first[0].digest.is_some());
        assert_eq!(hash(), first[0].hash(), "nothing edited: the same hash");
        std::fs::write(
            package.join("mise.toml"),
            "[tasks.serve]\nrun = \"false\"\n",
        )
        .unwrap();
        let second = hash();
        assert_ne!(second, first[0].hash(), "an edited mise.toml");
        std::fs::write(
            package.join("balerix-plugin.yaml"),
            format!("{MANIFEST}# c\n"),
        )
        .unwrap();
        assert_ne!(hash(), second, "a manifest edit that parses the same");
    }

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

    /// The plugin agent `balerix/plugins/hello` in `phase`, its record
    /// stamped with the `hello` generation the actor last took.
    fn plugin_record(phase: Option<AgentPhase>, hello: u64) -> FleetRecord {
        let id = "balerix/plugins/hello";
        let mut record = record_with(phase.map(|_| id));
        if let Some(phase) = phase {
            let a = record.status.entry(id);
            a.phase = phase;
            a.plugin_hello = hello;
        }
        record
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    async fn ready_becomes(reg: &PluginRegistry, name: &AgentName, want: bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while reg.plugin(name).is_some_and(|p| p.ready) != want {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("ready never became {want}"));
    }

    /// #12: `watch` coalesces publishes, so the watcher can see a plugin
    /// dead without ever having seen it `Ready`. A terminal record that
    /// carries the newest `hello` takes readiness back; one that predates
    /// the newest `hello` (a restarted plugin that already said hello
    /// again) does not.
    #[tokio::test]
    async fn a_dead_plugin_seen_only_after_a_coalesced_ready_is_not_ready() {
        let reg = PluginRegistry::new();
        let name: AgentName = "hello".parse().unwrap();
        reg.declare(&[(name.clone(), serde_json::json!({}))], &[]);
        let g1 = reg.set_listen(&name, "127.0.0.1:1".into(), "t".into());
        assert!(reg.plugin(&name).unwrap().ready);
        // Ready (stamped g1), then Dead: only the Dead is ever observed.
        let (tx, rx) = watch::channel(plugin_record(Some(AgentPhase::Dead), g1));
        tokio::spawn(mirror_readiness(rx, reg.clone()));
        ready_becomes(&reg, &name, false).await;

        // A new process said hello (g2) before the actor published past
        // the old death: that record must not silence it.
        let g2 = reg.set_listen(&name, "127.0.0.1:2".into(), "t".into());
        assert!(g2 > g1);
        tx.send_replace(plugin_record(Some(AgentPhase::Dead), g1));
        settle().await;
        assert!(reg.plugin(&name).unwrap().ready, "a stale death is ignored");
        tx.send_replace(plugin_record(Some(AgentPhase::Starting), g1));
        settle().await;
        assert!(reg.plugin(&name).unwrap().ready);

        // The actor took g2 and the plugin died again; coalesced again.
        tx.send_replace(plugin_record(Some(AgentPhase::Stopped), g2));
        ready_becomes(&reg, &name, false).await;

        // Ready then gone from the record, coalesced into the removal.
        let g3 = reg.set_listen(&name, "127.0.0.1:3".into(), "t".into());
        tx.send_replace(plugin_record(Some(AgentPhase::Ready), g3));
        settle().await;
        assert!(reg.plugin(&name).unwrap().ready);
        tx.send_replace(plugin_record(None, 0));
        ready_becomes(&reg, &name, false).await;
    }
}
