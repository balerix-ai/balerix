//! The registry (Phase 3 spec §3.1): fleet name → actor handle, the shared
//! secret index, and the request-side logic the API calls into.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use balerix_api::{
    ActivateRequest, ActivationState, CredentialBundle, DeactivateRequest, Desired, FleetSpec,
    FleetSummary, HelloRequest, HelloResponse, HookEvent, PluginAction, PluginActivation,
    SyncReport,
};
use balerix_core::{
    AgentId, AgentName, AgentRunner, EventHandler, Fleet, FleetName, FleetRecord, FleetSecrets,
    Keep, Outcome, ResolvedAgent, SystemToolchain, WorkspaceReader, is_reserved_fleet, plugin_id,
    reserved_fleet_reason,
};
use tokio::sync::{RwLock, mpsc, oneshot, watch};

use crate::actor::{self, FleetHandle, Msg, Ports, Shared};
use crate::auth::constant_time_eq;
use crate::hooks::ParsedEvent;
use crate::metrics::Metrics;
use crate::plugins::activation::{self, Pair};
use crate::plugins::{
    ActivationRow, CallFailure, PluginClient, PluginError, PluginHost, PluginKv, PluginRegistry,
    PluginSetup, PluginSource,
};
use crate::sessions::Sessions;
use crate::system_pool::{SystemPoolConfig, SystemPoolState};

/// The chain handler's hello hook; `PassThrough` has nothing to clear.
pub trait HelloObserver: Send + Sync {
    fn on_hello(&self, name: &AgentName);
}

impl HelloObserver for balerix_core::PassThrough {
    fn on_hello(&self, _: &AgentName) {}
}

impl HelloObserver for crate::plugins::PluginEventHandler {
    fn on_hello(&self, name: &AgentName) {
        crate::plugins::PluginEventHandler::on_hello(self, name);
    }
}

/// The one handler the daemon holds: the event chain plus its `hello` hook.
pub trait DaemonHandler: EventHandler + HelloObserver {}
impl<T: EventHandler + HelloObserver> DaemonHandler for T {}

/// `POST /v1/plugins/sync` and `DELETE /v1/plugins/{name}` in Kubernetes
/// mode (Spec O §23.2).
const KUBERNETES_PLUGINS: &str =
    "this daemon is in kubernetes mode; change its plugins through the Daemon's spec.plugins";

/// How often every ready plugin's `GET /v1/health` is polled (§16.5).
pub const HEALTH_INTERVAL: Duration = Duration::from_secs(10);
/// How long `apply` and the purge listener wait for an actor that has
/// purged its fleet to end (#10); it returns right after, so this is a
/// bound on a bug, not a timing.
const PURGE_EXIT_WAIT: Duration = Duration::from_secs(10);
/// How many times `hello`'s re-activation offers one row while applies
/// keep changing it under it (#10).
const REACTIVATE_TRIES: usize = 5;

pub struct Daemon {
    fleets: RwLock<BTreeMap<FleetName, FleetHandle>>,
    ports: Arc<Ports>,
    shared: Shared,
    handler: Arc<dyn DaemonHandler>,
    token: String,
    plugins: PluginSource,
    /// Kubernetes mode's stored managed fleet requests (Spec O §23.3);
    /// `None` on one machine, where the Daemon applies them itself.
    managed: Option<Arc<crate::kube::managed::ManagedStore>>,
    registry: Arc<PluginRegistry>,
    client: PluginClient,
    /// The reverse proxy's own connection pool for the plugin mount.
    proxy_client: crate::proxy::HttpClient,
    kv: Arc<PluginKv>,
    sessions: Sessions,
    /// Ticks once per published actor snapshot and once per registry
    /// write; `fleets/watch` waits on it (§18.4).
    changes: Arc<watch::Sender<u64>>,
    /// One `apply` or `down` at a time *per fleet*: activation and the
    /// actor message must not interleave with another apply of the same
    /// fleet. Every other fleet runs on its own lock — an apply waits for
    /// the actor's pass, and one fleet's pass must not hold up the rest.
    applying: std::sync::Mutex<BTreeMap<FleetName, Arc<tokio::sync::Mutex<()>>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DaemonError {
    #[error("fleet not found")]
    NotFound,
    #[error("fleet exists; use `balerix update`, or `balerix down` first")]
    Conflict,
    #[error("{0}")]
    Invalid(String),
    #[error("unknown agent or bad secret")]
    Unauthorized,
    #[error("{0}")]
    Internal(String),
    /// The owner rule (Spec L §5): a 409 naming who manages the fleet.
    #[error("{0}")]
    Managed(String),
    /// Spec O §23.8: a plugin's hello names a revision the Daemon's list
    /// does not hold yet. 409, and nothing changes; the SDK retries.
    #[error("{0}")]
    ListPending(String),
}

/// Who is applying or downing a fleet (Spec L §5). The admin API and a
/// plugin with `manage` share `apply_as`/`down_as`; the owner rule reads
/// the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// The admin API. `force` lets `down` take a managed fleet
    /// (`balerix down --force`, and the operator's own `DELETE`); nothing
    /// lets the admin apply one.
    Admin { force: bool },
    /// A plugin with `manage`, by name.
    Plugin(AgentName),
    /// The operator (Spec O §7.3): a `PUT` carrying `agent_tokens`, and
    /// with `managed_by` the plugin it applies a managed fleet for
    /// (§23.3).
    Kubernetes { managed_by: Option<AgentName> },
}

impl Caller {
    fn owner(&self) -> Option<String> {
        match self {
            Caller::Admin { .. } => None,
            Caller::Plugin(p) => Some(p.to_string()),
            // the record of a managed fleet keeps the plugin as its owner
            Caller::Kubernetes {
                managed_by: Some(p),
            } => Some(p.to_string()),
            Caller::Kubernetes { managed_by: None } => {
                Some(crate::kube::KUBERNETES_OWNER.to_string())
            }
        }
    }
}

/// How an apply treats the record it finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyMode {
    /// `POST /v1/fleets`: 409 unless the fleet is absent or settled `Down`.
    Create,
    /// `PUT /v1/fleets/{name}`: 404 when absent.
    Replace,
    /// A plugin's `PUT` (Spec L §3.1): create or replace; a settled `Down`
    /// resumes.
    Upsert,
}

impl Daemon {
    /// Spawns one actor per stored fleet (each reconciles once), the
    /// listener that drops purged fleets and the health poller. Needs a
    /// tokio runtime.
    ///
    /// The wiring is wide because the daemon is where every port meets;
    /// grouping the plugin trio into a struct would only rename them.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        ports: Ports,
        handler: Arc<dyn DaemonHandler>,
        metrics: Metrics,
        token: String,
        existing: Vec<(FleetRecord, FleetSecrets)>,
        plugins: PluginSetup,
        registry: Arc<PluginRegistry>,
        client: PluginClient,
        kv: Arc<PluginKv>,
        system_toolchain: Arc<dyn SystemToolchain>,
    ) -> Arc<Self> {
        let (shared, purged, pool_tx) = actor::shared(metrics);
        // Spec F §4: one owner for the daemon pool. Spawned before the fleet
        // actors, though they gate on readiness rather than on spawn order.
        crate::system_pool::spawn(system_toolchain, pool_tx, SystemPoolConfig::default());
        let (plugins, managed) = match plugins {
            PluginSetup::Packages(config) => (
                PluginSource::Packages(PluginHost::start(
                    config,
                    &ports,
                    shared.clone(),
                    registry.clone(),
                )),
                None,
            ),
            // Kubernetes mode launches nothing: the operator runs the
            // plugins and sends their list (§23.2), and applies their
            // fleets from the stored requests (§23.3).
            PluginSetup::Declared {
                state_dir,
                managed_dir,
            } => (
                PluginSource::Declared(crate::kube::DeclaredPlugins::new(
                    state_dir,
                    registry.clone(),
                )),
                Some(crate::kube::managed::ManagedStore::load(managed_dir)),
            ),
        };
        let ports = Arc::new(ports);
        let changes = Arc::new(watch::channel(0u64).0);
        let mut fleets = BTreeMap::new();
        for (record, secrets) in existing {
            match FleetName::try_from(record.spec.name.clone()) {
                // The plugin host owns `balerix` and already has an actor;
                // a stored record under a reserved name predates the
                // reservation (or was written by hand) and would fight it
                // for tmux and state, or could never be fetched.
                Ok(name) if reserved_fleet_reason(name.as_str()).is_some() => tracing::error!(
                    fleet = %name,
                    "ignoring a stored fleet named {name}: the name is {}",
                    reserved_fleet_reason(name.as_str()).unwrap_or_default()
                ),
                Ok(name) => {
                    // Nothing about activation is persisted (§16.2): every
                    // pair of a stored fleet starts pending and the
                    // plugin's next `hello` activates it.
                    if matches!(record.desired, Desired::Up) {
                        for p in activation::pairs(&name, &record.spec).unwrap_or_default() {
                            registry.set_row(
                                &p.agent,
                                &p.plugin,
                                ActivationRow {
                                    config: p.config,
                                    activation: PluginActivation::pending(),
                                },
                            );
                        }
                    }
                    let h = actor::spawn(
                        name.clone(),
                        record,
                        secrets,
                        ports.clone(),
                        shared.clone(),
                        true,
                    );
                    tokio::spawn(Self::forward_changes(h.status.clone(), changes.clone()));
                    fleets.insert(name, h);
                }
                Err(e) => tracing::error!("skipping a stored fleet with an invalid name: {e}"),
            }
        }
        let proxy_client = crate::proxy::client(client.tls());
        let daemon = Arc::new(Self {
            fleets: RwLock::new(fleets),
            ports,
            shared,
            handler,
            token,
            plugins,
            managed,
            registry,
            client,
            proxy_client,
            kv,
            sessions: Sessions::new(),
            changes,
            applying: std::sync::Mutex::new(BTreeMap::new()),
        });
        tokio::spawn(Self::forget_purged(Arc::downgrade(&daemon), purged));
        tokio::spawn(Self::health_loop(Arc::downgrade(&daemon)));
        daemon
    }

    async fn health_loop(daemon: std::sync::Weak<Self>) {
        loop {
            tokio::time::sleep(HEALTH_INTERVAL).await;
            let Some(d) = daemon.upgrade() else {
                return;
            };
            d.poll_health().await;
        }
    }

    /// One round: every ready plugin's `/v1/health`; a failure sets its
    /// degraded message, a success clears it. Never restarts anything.
    /// A declared plugin's readiness itself comes from here (§23.2).
    pub async fn poll_health(&self) {
        if let Some(d) = self.plugins.declared() {
            for (name, addr) in d.pollable() {
                let result = self.client.health(&addr).await.map_err(|e| e.to_string());
                if let Err(e) = &result {
                    tracing::warn!(plugin = %name, "health check failed: {e}");
                }
                if d.health(&name, result) {
                    self.reactivate(&name).await;
                    self.bump();
                }
            }
            return;
        }
        for name in self.registry.names() {
            let Some(addr) = self.registry.ready_addr(&name) else {
                continue;
            };
            match self.client.health(&addr).await {
                Ok(()) => self.registry.set_degraded(&name, None),
                Err(e) => {
                    tracing::warn!(plugin = %name, "health check failed: {e}");
                    self.registry.set_degraded(&name, Some(e.to_string()));
                }
            }
        }
    }

    async fn forget_purged(daemon: std::sync::Weak<Self>, mut purged: mpsc::Receiver<FleetName>) {
        while let Some(name) = purged.recv().await {
            let Some(d) = daemon.upgrade() else {
                return;
            };
            // One task per fleet: a forget waits for its fleet's lock, and
            // must not hold up another fleet's behind it.
            tokio::spawn(async move { d.forget_one(&name).await });
        }
    }

    /// Drops a purged fleet's handle (#10), under the fleet's lock so it
    /// cannot interleave with an `apply` or `down` of the same name, and
    /// only while the entry is still the purged actor's: an `apply` that
    /// got the lock first has already put a new actor in its place, and
    /// that fleet stays.
    async fn forget_one(&self, name: &FleetName) {
        let lock = self.fleet_lock(name);
        let _guard = lock.lock().await;
        let entry = self.fleets.read().await.get(name).cloned();
        if let Some(h) = entry {
            if !Self::wait_purged(name, &h).await {
                return;
            }
            self.fleets.write().await.remove(name);
        }
        if let Some(m) = &self.managed {
            m.forget(name.as_str());
        }
        self.bump();
    }

    /// `true` once `h`'s actor has ended after purging its fleet; `false`
    /// at once for a live actor that is not purging. The actor tells the
    /// purge listener before its task returns, and a settled `Down` with
    /// `purge` is the last record it publishes, so a handle that shows
    /// one is about to close: this waits for it (bounded, in case it never
    /// does) rather than hand that actor a message it would drop.
    async fn wait_purged(name: &FleetName, h: &FleetHandle) -> bool {
        let purging = {
            let r = h.status.borrow();
            matches!(r.desired, Desired::Down { purge: true, .. }) && r.is_down()
        };
        if !purging && !h.tx.is_closed() {
            return false;
        }
        if tokio::time::timeout(PURGE_EXIT_WAIT, h.tx.closed())
            .await
            .is_err()
        {
            tracing::error!(fleet = %name, "purged fleet's actor did not end");
            return false;
        }
        true
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn metrics(&self) -> &Metrics {
        &self.shared.metrics
    }

    /// The one-machine plugin host; `None` in Kubernetes mode.
    pub fn plugin_host(&self) -> Option<&Arc<PluginHost>> {
        self.plugins.packages()
    }

    pub fn registry(&self) -> &Arc<PluginRegistry> {
        &self.registry
    }

    pub fn client(&self) -> &PluginClient {
        &self.client
    }

    pub fn proxy_client(&self) -> &crate::proxy::HttpClient {
        &self.proxy_client
    }

    pub fn kv(&self) -> &Arc<PluginKv> {
        &self.kv
    }

    pub fn sessions(&self) -> &Sessions {
        &self.sessions
    }

    /// The daemon's own origin, `http://127.0.0.1:<port>`: the login URL's
    /// host and the only `Origin` a cookie request may carry (§18.2).
    pub fn origin(&self) -> &str {
        &self.ports.hook_url
    }

    /// Activation state is a read-time overlay (§16.3): the actor never
    /// holds it, every record leaves through here.
    fn overlay(&self, mut record: FleetRecord) -> FleetRecord {
        self.registry.overlay(&mut record);
        record
    }

    /// Every published snapshot of one actor becomes one tick of the
    /// change counter `fleets/watch` waits on; a final tick when the actor
    /// ends (a purge), so the list without it goes out too.
    async fn forward_changes(
        mut status: watch::Receiver<FleetRecord>,
        changes: Arc<watch::Sender<u64>>,
    ) {
        while status.changed().await.is_ok() {
            changes.send_modify(|n| *n += 1);
        }
        changes.send_modify(|n| *n += 1);
    }

    /// Ticks when any fleet record or activation row changed (§18.4).
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Registry writes have no actor behind them; the writer ticks.
    fn bump(&self) {
        self.changes.send_modify(|n| *n += 1);
    }

    pub fn runner(&self) -> Arc<dyn AgentRunner> {
        self.ports.runner.clone()
    }

    pub fn workspace(&self) -> Arc<dyn WorkspaceReader> {
        self.ports.workspace.clone()
    }

    /// Spec O §7: the link hub, when this daemon is in Kubernetes mode.
    pub fn kube(&self) -> Option<&Arc<crate::kube::LinkHub>> {
        self.ports.kube.as_ref()
    }

    /// A sidecar's `status` frame, to its fleet's actor. An unknown fleet
    /// (the operator deleted it while the pod lived) is dropped.
    /// `first` is true for the first frame of a link (a connect or a
    /// reconnect): the one the actor reconciles the stopped set against.
    pub async fn link_status(&self, agent: &AgentId, status: balerix_api::LinkStatus, first: bool) {
        if let Some(h) = self.fleets.read().await.get(&agent.fleet) {
            let _ =
                h.tx.send(Msg::LinkStatus {
                    agent: agent.clone(),
                    status,
                    first,
                })
                .await;
        }
    }

    pub async fn link_down(&self, agent: &AgentId) {
        if let Some(h) = self.fleets.read().await.get(&agent.fleet) {
            let _ =
                h.tx.send(Msg::LinkDown {
                    agent: agent.clone(),
                })
                .await;
        }
    }

    /// `origin/<ref>` of the agent's crew, from the fleet's record; `None`
    /// when the fleet or the crew is unknown.
    pub async fn base_ref(&self, agent: &AgentId) -> Option<String> {
        let record = self.get(&agent.fleet).await?;
        let crew = record.spec.crews.get(agent.crew.as_str())?;
        Some(format!("origin/{}", crew.git_ref))
    }

    /// Reconciles the plugin set to `plugins.yaml`; `serve` calls it once
    /// at start and fails fast on an error, `plugin sync` on demand. A
    /// plugin that is not declared cannot down the fleets it owns, and
    /// agents nobody can talk to are the wrong default (Spec L-6): every
    /// owned fleet still up whose owner the file does not declare is
    /// downed here, every failure reported, and the sync goes on. That
    /// covers a plugin this sync stopped and one removed while the daemon
    /// was stopped (the first sync at start has stopped nothing).
    ///
    /// The declared set is the resolved one: `PluginHost::sync` resolves
    /// every entry or fails whole, before anything here runs, so a
    /// transient install failure downs nothing.
    pub async fn sync_plugins(&self) -> Result<SyncReport, PluginError> {
        let Some(host) = self.plugins.packages() else {
            return Err(PluginError::Managed(KUBERNETES_PLUGINS.into()));
        };
        let mut report = host.sync().await?;
        let declared: BTreeSet<&str> = report
            .installed
            .iter()
            .chain(&report.unchanged)
            .map(String::as_str)
            .collect();
        let mut downed = Vec::new();
        let mut down_failed = Vec::new();
        for record in self.plugin_fleets().await {
            let Some(plugin) = record.owner.as_deref() else {
                continue;
            };
            // the operator's fleets have no plugin behind them (Spec O §7.3)
            if plugin == crate::kube::KUBERNETES_OWNER
                || declared.contains(plugin)
                || matches!(record.desired, Desired::Down { .. })
            {
                continue;
            }
            let Ok(name) = FleetName::try_from(record.spec.name.clone()) else {
                continue;
            };
            tracing::warn!(
                fleet = %name,
                plugin,
                "downing fleet {name}: its plugin {plugin} is not declared in plugins.yaml"
            );
            match self
                .down_as(
                    &name,
                    Keep::default(),
                    false,
                    &Caller::Admin { force: true },
                )
                .await
            {
                Ok(_) => downed.push(name.to_string()),
                Err(e) => {
                    tracing::warn!(fleet = %name, plugin, "downing a removed plugin's fleet failed: {e}");
                    down_failed.push(format!("{name}: {e}"));
                }
            }
        }
        report.downed = downed;
        report.down_failed = down_failed;
        // `replace_plugins` drops the activation rows of every plugin the
        // sync removed, and the plugin fleet's own actor snapshot is not
        // forwarded to `changes` — so this registry write ticks like the
        // others, or `fleets/watch` keeps serving the removed rows. A
        // sync that fails resolving does so before it replaces anything;
        // the failures after `replace_plugins` are the actor being gone,
        // which is shutdown, when no watcher is left to tell.
        self.bump();
        Ok(report)
    }

    /// `plugin remove --purge`; refused in Kubernetes mode, where the
    /// operator's list is the only way to change the plugin set.
    pub async fn purge_plugin(&self, name: &AgentName) -> Result<(), PluginError> {
        let Some(host) = self.plugins.packages() else {
            return Err(PluginError::Managed(KUBERNETES_PLUGINS.into()));
        };
        host.purge(name).await
    }

    /// `PUT /v1/plugins` (Spec O §23.2). The rows of a dropped plugin go
    /// with it; its stored managed requests too (§23.3).
    pub async fn declare_plugins(
        &self,
        list: balerix_api::DeclaredPlugins,
    ) -> Result<Vec<AgentName>, DaemonError> {
        let Some(d) = self.plugins.declared() else {
            return Err(DaemonError::Managed(
                "this daemon reads plugins.yaml".into(),
            ));
        };
        // Refused up front: accepting the list would only fail every call
        // later with an opaque TLS error (§23.1).
        if !self.client.trusts() {
            return Err(DaemonError::Managed(
                "this daemon was started without --tls-ca; it cannot call plugins".into(),
            ));
        }
        let listed: Vec<String> = list.plugins.iter().map(|p| p.name.clone()).collect();
        let dropped = d
            .replace(list.plugins)
            .map_err(|e| DaemonError::Invalid(e.to_string()))?;
        if let Some(m) = &self.managed {
            let listed: Vec<&str> = listed.iter().map(String::as_str).collect();
            m.retain_plugins(&listed);
        }
        self.bump();
        Ok(dropped)
    }

    /// `GET /v1/managed-fleets` (Spec O §23.3): the plugins' stored
    /// requests, for the operator.
    pub fn managed_fleets(&self) -> Result<Vec<balerix_api::ManagedFleet>, DaemonError> {
        self.managed
            .as_ref()
            .map(|m| m.list())
            .ok_or_else(|| DaemonError::Managed("this daemon reads plugins.yaml".into()))
    }

    /// A plugin downed a managed fleet: its stored request says so
    /// (§23.3). Nothing to do on one machine.
    pub fn managed_down(&self, name: &FleetName, q: balerix_api::DownQuery) {
        if let Some(m) = &self.managed {
            m.mark_down(name.as_str(), q);
        }
    }

    /// `GET /v1/plugins`: one row per plugin, from whichever source.
    pub async fn list_plugins(&self) -> Vec<balerix_api::PluginStatus> {
        match &self.plugins {
            PluginSource::Packages(host) => host.list().await,
            PluginSource::Declared(d) => d.list(|n| self.registry.active_agents(n)),
        }
    }

    /// `hello` authenticates with the plugin's token — on one machine the
    /// hook secret the actor minted for `balerix/plugins/<name>`, in
    /// Kubernetes mode the list entry's — and is otherwise the plugin's
    /// `SessionStart`.
    pub async fn plugin_hello(
        &self,
        name: &AgentName,
        token: &str,
        req: HelloRequest,
    ) -> Result<HelloResponse, DaemonError> {
        let response = match &self.plugins {
            PluginSource::Packages(host) => {
                if !self.verify_secret(&plugin_id(name), token).await {
                    return Err(DaemonError::Unauthorized);
                }
                host.hello(name, req, token).await?
            }
            PluginSource::Declared(d) => {
                if d.plugin_for_token(token).as_ref() != Some(name) {
                    return Err(DaemonError::Unauthorized);
                }
                d.hello(name, &req)?
            }
        };
        self.handler.on_hello(name);
        self.reactivate(name).await;
        self.bump();
        Ok(response)
    }

    /// Restart recovery (§16.2): the plugin knows nothing about the
    /// pairs it had; every row is offered again, and a refusal now is
    /// the pair's state, not an error for the plugin.
    ///
    /// Not under the fleets' apply locks: a plugin whose `activate` calls
    /// `PUT fleets/{f}` would wait on its own `hello`. An apply can
    /// therefore change a row while this runs (#10). Each row is read
    /// again right before its `activate`, so it is offered its config of
    /// now, and an answer is written only onto a row that still holds the
    /// config it answered (`set_state`). When that fails, the apply's
    /// `activate` may have reached the plugin before this one did, so the
    /// row's current config is offered again — `activate` replaces in
    /// place, so this converges — and a row the apply removed is
    /// deactivated, at most `REACTIVATE_TRIES` offers per row.
    async fn reactivate(&self, name: &AgentName) {
        let Some(addr) = self.registry.ready_addr(name) else {
            return;
        };
        for (agent, _) in self.registry.rows_for_plugin(name) {
            let mut offered = false;
            for _ in 0..REACTIVATE_TRIES {
                let Some(row) = self.registry.row(&agent, name) else {
                    if offered {
                        self.deactivate_pair(&agent, name).await;
                    }
                    break;
                };
                let req = ActivateRequest {
                    agent: agent.to_string(),
                    config: row.config.clone(),
                };
                offered = true;
                let activation = match self.client.activate(&addr, &req).await {
                    Ok(()) => PluginActivation::active(),
                    Err(e) => {
                        tracing::warn!(plugin = %name, agent = %agent, "activation rejected at hello: {e}");
                        PluginActivation::rejected(Self::activation_message(&e))
                    }
                };
                if self
                    .registry
                    .set_state(&agent, name, &row.config, activation)
                {
                    offered = false;
                    break;
                }
                tracing::info!(plugin = %name, agent = %agent, "row changed during re-activation; offering its current config");
            }
            if offered {
                tracing::warn!(plugin = %name, agent = %agent, "row kept changing during re-activation; the next hello offers it again");
            }
        }
    }

    /// `CallFailure::Status` carries the plugin's own error verbatim; every
    /// other failure is described, never trusted as a message.
    fn activation_message(e: &CallFailure) -> String {
        match e {
            CallFailure::Status { message, .. } => message.clone(),
            other => format!("plugin unreachable ({other})"),
        }
    }

    /// This fleet's apply/down lock, created on first use.
    fn fleet_lock(&self, name: &FleetName) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.applying.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(name.clone()).or_default().clone()
    }

    /// Best effort: a plugin that cannot be told is logged, not an error.
    async fn deactivate_pair(&self, agent: &AgentId, plugin: &AgentName) {
        let Some(addr) = self.registry.ready_addr(plugin) else {
            return;
        };
        let req = DeactivateRequest {
            agent: agent.to_string(),
        };
        if let Err(e) = self.client.deactivate(&addr, &req).await {
            tracing::warn!(plugin = %plugin, agent = %agent, "deactivate failed: {e}");
        }
    }

    /// Puts back the pairs a rejected apply had already replaced with a new
    /// config: their rows still say `Active` with the old config, so the
    /// plugin is told to hold that again. A refusal now is the row's new
    /// state — the pair really is not active any more.
    async fn restore_pairs(&self, pairs: &[Pair]) {
        for p in pairs {
            let Some(addr) = self.registry.ready_addr(&p.plugin) else {
                continue;
            };
            let req = ActivateRequest {
                agent: p.agent.to_string(),
                config: p.config.clone(),
            };
            if let Err(e) = self.client.activate(&addr, &req).await {
                tracing::warn!(plugin = %p.plugin, agent = %p.agent, "restoring the previous activation failed: {e}");
                self.registry.set_state(
                    &p.agent,
                    &p.plugin,
                    &p.config,
                    PluginActivation::rejected(Self::activation_message(&e)),
                );
            }
        }
    }

    /// The `balerix` fleet belongs to the plugin host: readable, never
    /// written through the fleet API. `watch` would shadow a route.
    fn reject_reserved(name: &FleetName) -> Result<(), DaemonError> {
        if let Some(why) = reserved_fleet_reason(name.as_str()) {
            return Err(DaemonError::Invalid(format!(
                "name: {:?} is {why}",
                name.as_str()
            )));
        }
        Ok(())
    }

    /// Spec L §5: the CLI and a plugin never touch each other's fleets.
    /// `downing` is the one case an admin may override, with `force`.
    fn check_owner(
        name: &FleetName,
        owner: Option<&str>,
        caller: &Caller,
        downing: bool,
    ) -> Result<(), DaemonError> {
        let managed = |p: &str| {
            DaemonError::Managed(if p == crate::kube::KUBERNETES_OWNER {
                format!("fleet {name} is managed by kubernetes; change it through its Fleet object")
            } else {
                format!("fleet {name} is managed by plugin {p}")
            })
        };
        match (caller, owner) {
            (Caller::Admin { force }, Some(p)) if !(downing && *force) => Err(managed(p)),
            (Caller::Plugin(me), Some(p)) if p != me.as_str() => Err(managed(p)),
            (Caller::Plugin(_), None) => Err(DaemonError::Managed(format!(
                "fleet {name} is not managed by a plugin"
            ))),
            // §23.3: the operator acting for a plugin applies or downs
            // only that plugin's fleets
            (
                Caller::Kubernetes {
                    managed_by: Some(me),
                },
                Some(p),
            ) if p != me.as_str() => Err(managed(p)),
            // nor adopts a record nobody manages, a CLI one (R7)
            (
                Caller::Kubernetes {
                    managed_by: Some(_),
                },
                None,
            ) => Err(DaemonError::Managed(format!(
                "fleet {name} is not managed by a plugin"
            ))),
            (
                Caller::Kubernetes {
                    managed_by: Some(_),
                },
                Some(_),
            ) => Ok(()),
            (Caller::Kubernetes { managed_by: None }, Some(p))
                if p != crate::kube::KUBERNETES_OWNER =>
            {
                Err(managed(p))
            }
            // the operator never adopts a record it did not create
            (Caller::Kubernetes { managed_by: None }, None) => Err(DaemonError::Managed(format!(
                "fleet {name} is not managed by kubernetes"
            ))),
            _ => Ok(()),
        }
    }

    /// The admin API's apply: `POST` (`replace == false`) or `PUT`.
    pub async fn apply(
        &self,
        name: &FleetName,
        spec: FleetSpec,
        credentials: CredentialBundle,
        replace: bool,
    ) -> Result<FleetRecord, DaemonError> {
        let mode = if replace {
            ApplyMode::Replace
        } else {
            ApplyMode::Create
        };
        self.apply_as(
            name,
            spec,
            credentials,
            mode,
            &Caller::Admin { force: false },
            BTreeMap::new(),
        )
        .await
    }

    /// Spec O §7.3: the operator's apply. A resolved spec plus one token
    /// per agent, which becomes the agent's hook secret (its sidecar
    /// presents it on the hook route and on the link). An upsert: the
    /// operator re-sends on every reconcile. The tokens are checked
    /// against the spec before the owner rule or any plugin is consulted.
    /// `managed_by` names the plugin a managed fleet is applied for
    /// (§23.3); its record keeps that plugin as owner.
    pub async fn apply_kube(
        &self,
        name: &FleetName,
        spec: FleetSpec,
        agent_tokens: balerix_api::AgentTokens,
        managed_by: Option<AgentName>,
    ) -> Result<FleetRecord, DaemonError> {
        if self.ports.kube.is_none() {
            return Err(DaemonError::Invalid(
                "agent_tokens is accepted only by a daemon in kubernetes mode".into(),
            ));
        }
        let fleet =
            Fleet::try_from(spec.clone()).map_err(|e| DaemonError::Invalid(e.to_string()))?;
        let wanted: Vec<String> = ResolvedAgent::from_fleet(&fleet)
            .into_iter()
            .map(|a| a.id.to_string())
            .collect();
        for key in &wanted {
            match agent_tokens.get(key) {
                None => {
                    return Err(DaemonError::Invalid(format!(
                        "agent_tokens: no token for {key}"
                    )));
                }
                Some(t) if t.len() < 32 => {
                    return Err(DaemonError::Invalid(format!(
                        "agent_tokens.{key}: a token is at least 32 characters"
                    )));
                }
                Some(_) => {}
            }
        }
        for key in agent_tokens.keys() {
            if !wanted.contains(key) {
                return Err(DaemonError::Invalid(format!(
                    "agent_tokens: {key} is not an agent of the fleet"
                )));
            }
        }
        self.apply_as(
            name,
            spec,
            CredentialBundle::default(),
            ApplyMode::Upsert,
            &Caller::Kubernetes { managed_by },
            agent_tokens,
        )
        .await
    }

    /// Spec F's channel, for `/readyz`.
    pub fn system_pool_state(&self) -> SystemPoolState {
        self.shared.system_pool.borrow().clone()
    }

    /// `Create`: 409 unless the fleet is absent or settled `Down`.
    /// `Replace`: 404 when absent. `Upsert`: either. Every mode is
    /// answered before any plugin is called, so a refused apply leaves no
    /// plugin holding a config the fleet never took; and the owner rule
    /// (Spec L §5) is answered there too. A record a plugin creates
    /// carries that plugin as its owner.
    ///
    /// The activation diff's old side is the fleet's `Active` rows only
    /// (R24): a pair whose row is `Pending` or `Rejected` is offered to the
    /// plugin again by the next `apply`, even when its config is unchanged
    /// — a ready plugin gets an `activate` (and a rejection fails the
    /// apply like any other), a plugin that is not ready leaves it pending.
    pub async fn apply_as(
        &self,
        name: &FleetName,
        spec: FleetSpec,
        credentials: CredentialBundle,
        mode: ApplyMode,
        caller: &Caller,
        agent_tokens: balerix_api::AgentTokens,
    ) -> Result<FleetRecord, DaemonError> {
        Self::reject_reserved(name)?;
        if spec.name != name.as_str() {
            return Err(DaemonError::Invalid(format!(
                "spec.name {:?} does not match the fleet {name}",
                spec.name
            )));
        }
        Fleet::try_from(spec.clone()).map_err(|e| DaemonError::Invalid(e.to_string()))?;
        if self.ports.kube.is_none() {
            refuse_pod_runner(&spec)?;
        }
        // Activation runs before the actor sees the spec (§16.2): a
        // rejection is a 400 and nothing lands. Held for the whole method
        // so two applies of the same fleet cannot interleave.
        let lock = self.fleet_lock(name);
        let _guard = lock.lock().await;
        // Spec O §7.4: in Kubernetes mode a fleet runs only as pods the
        // operator made, so the CLI's apply of a fleet the operator does
        // not own (absent, or unowned) would wait for a `Ready` that never
        // comes. A kubernetes- or plugin-owned one gets the owner's 409
        // from `check_owner` below.
        if self.ports.kube.is_some() && matches!(caller, Caller::Admin { .. }) {
            let unowned = match self.fleets.read().await.get(name) {
                Some(h) => h.status.borrow().owner.is_none(),
                None => true,
            };
            if unowned {
                return Err(DaemonError::Managed(
                    "this daemon is in kubernetes mode; create a Fleet object".into(),
                ));
            }
        }
        // A plugin the registry no longer lists gets nothing (#62): its
        // `PUT` may have passed `manage_fleet`'s cheap owner check before
        // the operator removed it, and `sync_plugins` replaces the plugin
        // set before it snapshots the fleets to down. Checked under the
        // fleet's lock, so an apply that got in first finishes before the
        // sync's `down_as` of the same fleet, and one that arrives after
        // the replace is refused whether its fleet exists or not (hence
        // not inside `check_owner`, which only sees existing fleets). The
        // residual — an apply whose activation calls outlast the whole
        // plugin-stop pass and lands after the snapshot — is downed by
        // the next sync's undeclared-owner rule.
        if let Caller::Plugin(p) = caller
            && !self.registry.is_installed(p.as_str())
        {
            return Err(DaemonError::Managed(format!(
                "fleet {name}: plugin {p} is not installed"
            )));
        }
        // The fleet's rows *now*, not what its spec says: a `down` answers
        // before its teardown pass and has already dropped every row, so
        // the record's spec would name pairs that are gone.
        let rows_now = self.registry.rows_for_fleet(name);
        // Only the `Active` rows are the diff's old side (R24): a pending
        // or rejected pair is offered to the plugin again by the next
        // apply even when its config did not change.
        let old: Vec<Pair> = rows_now
            .iter()
            .filter(|(_, _, row)| row.activation.state == ActivationState::Active)
            .map(|(agent, plugin, row)| Pair {
                agent: agent.clone(),
                plugin: plugin.clone(),
                config: row.config.clone(),
            })
            .collect();
        let new =
            activation::pairs(name, &spec).map_err(|e| DaemonError::Invalid(e.to_string()))?;
        for p in &new {
            if !self.registry.is_installed(p.plugin.as_str()) {
                return Err(DaemonError::Invalid(format!(
                    "{}: no plugin {:?} is installed",
                    activation::config_path(&p.agent, &p.plugin),
                    p.plugin.as_str()
                )));
            }
        }
        // Whether this is a 409 (Create on a live fleet), a 404 (Replace
        // on an absent one) or the owner's 409 is decided *before* any
        // plugin is told anything: a rejected apply must not leave a
        // plugin holding a config the fleet never took. A purged fleet's
        // handle can still be in the map (the purge listener runs after
        // the actor has ended, and takes this lock to remove it): it is
        // waited out and counts as absent here and at the insert below,
        // and the listener leaves alone the actor this apply puts in its
        // place (#10). `apply` and `down` hold this lock and the listener
        // removes only a dead handle, so the write lock below sees the
        // same answer.
        let entry = self.fleets.read().await.get(name).cloned();
        if let Some(h) = entry {
            Self::wait_purged(name, &h).await;
        }
        {
            let fleets = self.fleets.read().await;
            match fleets.get(name).filter(|h| !h.tx.is_closed()) {
                Some(h) => {
                    let current = h.status.borrow();
                    Self::check_owner(name, current.owner.as_deref(), caller, false)?;
                    if mode == ApplyMode::Create && !current.is_down() {
                        return Err(DaemonError::Conflict);
                    }
                }
                None if mode == ApplyMode::Replace => return Err(DaemonError::NotFound),
                None => {}
            }
        }
        let mut d = activation::diff(&old, &new);
        // A row the new spec no longer names goes, whatever its state; the
        // diff only saw the active ones.
        for (agent, plugin, _) in &rows_now {
            let named = new.iter().any(|p| &p.agent == agent && &p.plugin == plugin);
            let already = d.deactivate.iter().any(|(a, p)| a == agent && p == plugin);
            if !named && !already {
                d.deactivate.push((agent.clone(), plugin.clone()));
            }
        }
        d.deactivate.sort();
        // every new or changed pair on a ready plugin is offered its config
        // by `activate` alone: a changed pair is replaced in place, never
        // deactivated first (§16.2, §17.9), so a rejection leaves the
        // plugin's state for it untouched. The first rejection rolls back
        // the pairs already accepted — one that was active before gets its
        // old config re-activated, a new one is deactivated — and nothing
        // reaches the actor.
        let mut accepted: Vec<Pair> = Vec::new();
        let mut rows: Vec<(Pair, PluginActivation)> = Vec::new();
        for p in &d.activate {
            match self.registry.ready_addr(&p.plugin) {
                Some(addr) => {
                    let req = ActivateRequest {
                        agent: p.agent.to_string(),
                        config: p.config.clone(),
                    };
                    match self.client.activate(&addr, &req).await {
                        Ok(()) => {
                            accepted.push(p.clone());
                            rows.push((p.clone(), PluginActivation::active()));
                        }
                        Err(e) => {
                            let previous = |a: &Pair| {
                                old.iter()
                                    .find(|o| o.agent == a.agent && o.plugin == a.plugin)
                                    .cloned()
                            };
                            let mut restore = Vec::new();
                            for a in &accepted {
                                match previous(a) {
                                    Some(was) => restore.push(was),
                                    None => self.deactivate_pair(&a.agent, &a.plugin).await,
                                }
                            }
                            self.restore_pairs(&restore).await;
                            // The rollback may have written rows of its own
                            // and this apply returns before the tick at the
                            // end: no actor snapshot is behind it either.
                            self.bump();
                            return Err(DaemonError::Invalid(format!(
                                "{}: {}",
                                activation::config_path(&p.agent, &p.plugin),
                                Self::activation_message(&e)
                            )));
                        }
                    }
                }
                // Not ready (#11): an unchanged row stays as it is — a
                // rejection keeps its message for `status`, a pending row
                // is not reset — and a changed config goes pending with
                // the last message carried over until a plugin answers.
                None => {
                    let was = rows_now
                        .iter()
                        .find(|(a, pl, _)| a == &p.agent && pl == &p.plugin)
                        .map(|(_, _, row)| row);
                    match was {
                        Some(row) if row.config == p.config => {}
                        Some(row) => rows.push((
                            p.clone(),
                            PluginActivation {
                                message: row.activation.message.clone(),
                                ..PluginActivation::pending()
                            },
                        )),
                        None => rows.push((p.clone(), PluginActivation::pending())),
                    }
                }
            }
        }
        let handle = {
            let mut fleets = self.fleets.write().await;
            match fleets.get(name).filter(|h| !h.tx.is_closed()) {
                Some(h) => {
                    if mode == ApplyMode::Create && !h.status.borrow().is_down() {
                        return Err(DaemonError::Conflict);
                    }
                    h.clone()
                }
                None => {
                    if mode == ApplyMode::Replace {
                        return Err(DaemonError::NotFound);
                    }
                    let h = actor::spawn(
                        name.clone(),
                        FleetRecord::with_owner(spec.clone(), caller.owner()),
                        FleetSecrets::default(),
                        self.ports.clone(),
                        self.shared.clone(),
                        false,
                    );
                    tokio::spawn(Self::forward_changes(
                        h.status.clone(),
                        self.changes.clone(),
                    ));
                    fleets.insert(name.clone(), h.clone());
                    h
                }
            }
        };
        // The rows are written before the actor hears of the spec: every
        // snapshot it publishes ticks `fleets/watch`, and a frame that
        // shows one of these agents must already carry its row (#113).
        // The overlay skips an agent the record does not have yet.
        for (p, activation) in &rows {
            self.registry.set_row(
                &p.agent,
                &p.plugin,
                ActivationRow {
                    config: p.config.clone(),
                    activation: activation.clone(),
                },
            );
        }
        let (reply, rx) = oneshot::channel();
        let sent = handle
            .tx
            .send(Msg::Apply {
                spec,
                credentials,
                agent_tokens,
                reply,
            })
            .await
            .map_err(|_| DaemonError::Internal("fleet task is gone".into()));
        let answered = match sent {
            Ok(()) => rx
                .await
                .map_err(|_| DaemonError::Internal("fleet task dropped the request".into())),
            Err(e) => Err(e),
        };
        let record = match answered {
            Ok(r) => r,
            Err(e) => {
                // The actor never took the spec: the rows go back to what
                // they were, a new pair's row away and a changed one's old.
                for (p, _) in &rows {
                    let was = rows_now
                        .iter()
                        .find(|(a, pl, _)| a == &p.agent && pl == &p.plugin);
                    match was {
                        Some((_, _, row)) => {
                            self.registry.set_row(&p.agent, &p.plugin, row.clone());
                        }
                        None => {
                            self.registry.remove_row(&p.agent, &p.plugin);
                        }
                    }
                }
                self.bump();
                return Err(e);
            }
        };
        // only the pairs the new spec dropped: a changed pair kept its row
        // and was replaced in place above
        for (agent, plugin) in &d.deactivate {
            self.deactivate_pair(agent, plugin).await;
            self.registry.remove_row(agent, plugin);
        }
        self.bump();
        Ok(self.overlay(record))
    }

    /// The admin API's down, without `--force`.
    pub async fn down(
        &self,
        name: &FleetName,
        keep: Keep,
        purge: bool,
    ) -> Result<FleetRecord, DaemonError> {
        self.down_as(name, keep, purge, &Caller::Admin { force: false })
            .await
    }

    /// Owner rule (Spec L §5): a plugin downs only what it owns; the admin
    /// needs `force` for a managed fleet. The owner survives the down.
    pub async fn down_as(
        &self,
        name: &FleetName,
        keep: Keep,
        purge: bool,
        caller: &Caller,
    ) -> Result<FleetRecord, DaemonError> {
        Self::reject_reserved(name)?;
        let lock = self.fleet_lock(name);
        let _guard = lock.lock().await;
        // A purged fleet whose handle the purge listener has not dropped
        // yet is gone already (#10).
        let handle = self
            .fleets
            .read()
            .await
            .get(name)
            .filter(|h| !h.tx.is_closed())
            .cloned()
            .ok_or(DaemonError::NotFound)?;
        Self::check_owner(name, handle.status.borrow().owner.as_deref(), caller, true)?;
        let (reply, rx) = oneshot::channel();
        handle
            .tx
            .send(Msg::Down { keep, purge, reply })
            .await
            .map_err(|_| DaemonError::Internal("fleet task is gone".into()))?;
        let record = rx
            .await
            .map_err(|_| DaemonError::Internal("fleet task dropped the request".into()))?;
        for (agent, plugin) in self.registry.remove_fleet(name) {
            self.deactivate_pair(&agent, &plugin).await;
        }
        self.bump();
        Ok(self.overlay(record))
    }

    /// `PUT /v1/plugin-host/fleets/{name}` (Spec L §3.1): a plugin's
    /// unresolved fleet file becomes a fleet the plugin owns. The name is
    /// checked first (reserved, and a `name` in the file must equal the
    /// path's), then the owner (cheaply, so a foreign fleet costs no
    /// resolve; `apply_as` checks again under the fleet's lock), then the
    /// plugin's `fleetDefaults` are read from the registry (a plugin it no
    /// longer lists is refused, 409, rather than resolved without them),
    /// then the file is resolved through the port beneath that layer and
    /// held to the restricted surface (Spec M §12.1)
    /// (400 with the resolver's message, config path first), the
    /// operator's credentials are read now (500: an unreadable host home
    /// is the operator's problem, not the plugin's), and the spec is
    /// applied as an upsert.
    pub async fn manage_fleet(
        &self,
        plugin: &AgentName,
        name: &FleetName,
        file: serde_json::Value,
    ) -> Result<FleetRecord, DaemonError> {
        Self::reject_reserved(name)?;
        if let Some(n) = file.get("name").and_then(serde_json::Value::as_str)
            && n != name.as_str()
        {
            return Err(DaemonError::Invalid(format!(
                "name: {n:?} does not match the fleet {name}"
            )));
        }
        let caller = Caller::Plugin(plugin.clone());
        if let Some(current) = self.get(name).await {
            Self::check_owner(name, current.owner.as_deref(), &caller, false)?;
        }
        // Fail closed: a plugin the registry no longer lists (a race with
        // `plugin remove` or a sync) has no `fleetDefaults` to resolve
        // beneath, and resolving without them would drop the operator's
        // sandbox and binary. `apply_as` refuses it too, under the lock.
        let layer = self
            .registry()
            .plugin(plugin)
            .map(|p| p.fleet_defaults)
            .ok_or_else(|| {
                DaemonError::Managed(format!("fleet {name}: plugin {plugin} is not installed"))
            })?;
        let resolver = self.ports.resolver.clone();
        let resolve_name = name.clone();
        // kept for the operator: in Kubernetes mode it resolves the file
        // into a Fleet itself (§23.3)
        let stored = self.managed.is_some().then(|| file.clone());
        let spec =
            tokio::task::spawn_blocking(move || resolver.resolve(&file, &resolve_name, &layer))
                .await
                .map_err(|e| DaemonError::Internal(e.to_string()))?
                .map_err(DaemonError::Invalid)?;
        let source = self.ports.credentials.clone();
        let credentials = tokio::task::spawn_blocking(move || source.load())
            .await
            .map_err(|e| DaemonError::Internal(e.to_string()))?
            .map_err(DaemonError::Internal)?;
        let record = self
            .apply_as(
                name,
                spec,
                credentials,
                ApplyMode::Upsert,
                &caller,
                BTreeMap::new(),
            )
            .await?;
        if let (Some(m), Some(file)) = (&self.managed, stored) {
            let declared = self.plugins.declared();
            let row = balerix_api::ManagedFleet {
                name: name.to_string(),
                plugin: plugin.to_string(),
                file,
                down: None,
            };
            // A list that dropped the plugin while this apply ran must not
            // leave its request behind for the operator (§23.2).
            if !m.put(row, |p| declared.is_some_and(|d| d.is_listed(p))) {
                tracing::warn!(fleet = %name, plugin = %plugin, "not storing a managed fleet: its plugin left the list");
            }
        }
        Ok(record)
    }

    pub async fn get(&self, name: &FleetName) -> Option<FleetRecord> {
        if is_reserved_fleet(name.as_str()) {
            return self.plugins.packages().map(|host| host.record());
        }
        self.fleets
            .read()
            .await
            .get(name)
            .map(|h| self.overlay(h.status.borrow().clone()))
    }

    /// Every record `/metrics` gauges, the plugin fleet included.
    pub async fn snapshots(&self) -> Vec<FleetRecord> {
        let mut out: Vec<FleetRecord> = self
            .fleets
            .read()
            .await
            .values()
            .map(|h| self.overlay(h.status.borrow().clone()))
            .collect();
        if let Some(host) = self.plugins.packages() {
            out.push(host.record());
        }
        out
    }

    /// User fleets with their activation rows: what the `fleets` route
    /// serves. Secrets never live in a record.
    pub async fn plugin_fleets(&self) -> Vec<FleetRecord> {
        self.fleets
            .read()
            .await
            .values()
            .map(|h| self.overlay(h.status.borrow().clone()))
            .collect()
    }

    /// User fleets only: the plugin fleet is not a fleet row.
    pub async fn list(&self) -> Vec<FleetSummary> {
        self.fleets
            .read()
            .await
            .values()
            .map(|h| h.status.borrow().summary())
            .collect()
    }

    pub async fn hook_secret(&self, agent: &AgentId) -> Option<String> {
        self.shared.hook_secrets.read().await.get(agent).cloned()
    }

    /// Which plugin presents this token: the `balerix` fleet's entries of
    /// the secret index, each compared in constant time. Plugins are few.
    pub async fn plugin_for_token(&self, token: &str) -> Option<AgentName> {
        if let Some(d) = self.plugins.declared() {
            return d.plugin_for_token(token);
        }
        let idx = self.shared.hook_secrets.read().await;
        idx.iter()
            .filter(|(id, _)| is_reserved_fleet(id.fleet.as_str()))
            .find(|(_, s)| constant_time_eq(s.as_bytes(), token.as_bytes()))
            .map(|(id, _)| id.agent.clone())
    }

    /// One action from a verdict or from the `actions` route. `plugin` is
    /// the metrics label when a plugin asked for it.
    pub async fn execute_action(
        &self,
        agent: &AgentId,
        action: &PluginAction,
        plugin: Option<&str>,
    ) -> Result<(), DaemonError> {
        action.validate().map_err(DaemonError::Invalid)?;
        self.shared.metrics.hook_action(agent, action.label());
        if let Some(p) = plugin {
            self.shared.metrics.plugin_action(p, action.label());
        }
        match action {
            PluginAction::SendText { text, submit } => {
                let runner = self.ports.runner.clone();
                let (id, text, submit) = (agent.clone(), text.clone(), *submit);
                tokio::task::spawn_blocking(move || runner.send_text(&id, &text, submit))
                    .await
                    .map_err(|e| DaemonError::Internal(e.to_string()))?
                    .map_err(|e| DaemonError::Internal(e.to_string()))
            }
            PluginAction::Stop => self.set_stopped(agent, true).await.map(|_| ()),
            PluginAction::Restart => {
                self.set_stopped(agent, true).await?;
                self.set_stopped(agent, false).await.map(|_| ())
            }
            PluginAction::SendKeys { steps, delay_ms } => {
                let runner = self.ports.runner.clone();
                let (id, steps) = (agent.clone(), steps.clone());
                let delay = std::time::Duration::from_millis(*delay_ms);
                tokio::task::spawn_blocking(move || runner.send_keys(&id, &steps, delay))
                    .await
                    .map_err(|e| DaemonError::Internal(e.to_string()))?
                    .map_err(|e| DaemonError::Internal(e.to_string()))
            }
        }
    }

    /// Holds (or releases) one agent in the fleet's `stopped` set; the
    /// actor replies after its pass, so `restart` is two ordered trips.
    async fn set_stopped(
        &self,
        agent: &AgentId,
        stopped: bool,
    ) -> Result<FleetRecord, DaemonError> {
        let handle = self
            .fleets
            .read()
            .await
            .get(&agent.fleet)
            .cloned()
            .ok_or(DaemonError::NotFound)?;
        let (reply, rx) = oneshot::channel();
        handle
            .tx
            .send(Msg::SetStopped {
                agent: agent.clone(),
                stopped,
                reply,
            })
            .await
            .map_err(|_| DaemonError::Internal("fleet task is gone".into()))?;
        rx.await
            .map_err(|_| DaemonError::Internal("fleet task dropped the request".into()))
    }

    /// Runs a verdict's actions in order after the response was written
    /// (architecture spec §8). Failures are logged; Claude already moved on.
    pub async fn run_actions(self: Arc<Self>, agent: AgentId, actions: Vec<PluginAction>) {
        for a in &actions {
            if let Err(e) = self.execute_action(&agent, a, None).await {
                tracing::warn!(agent = %agent, action = a.label(), "action failed: {e}");
            }
        }
    }

    /// Constant-time compare against the agent's current secret. `false`
    /// for an unknown agent and a wrong secret alike. Ingress calls this
    /// *before* the rate limiter (spec §3.5): an unauthenticated caller
    /// must never be able to touch — let alone drain or grow — another
    /// agent's bucket.
    pub async fn verify_secret(&self, agent: &AgentId, secret: &str) -> bool {
        match self.hook_secret(agent).await {
            Some(s) => constant_time_eq(s.as_bytes(), secret.as_bytes()),
            None => false,
        }
    }

    /// Authenticates, forwards the event to the fleet, runs the handler.
    /// Unknown agent and bad secret are the same error on purpose.
    pub async fn event(
        &self,
        agent: &AgentId,
        secret: &str,
        event: ParsedEvent,
    ) -> Result<Outcome, DaemonError> {
        let expected = self.hook_secret(agent).await;
        match expected {
            Some(s) if constant_time_eq(s.as_bytes(), secret.as_bytes()) => {}
            _ => return Err(DaemonError::Unauthorized),
        }
        let handle = self
            .fleets
            .read()
            .await
            .get(&agent.fleet)
            .cloned()
            .ok_or(DaemonError::Unauthorized)?;
        let started = Instant::now();
        let at = self.ports.clock.now();
        handle
            .tx
            .send(Msg::Event {
                agent: agent.clone(),
                name: event.name.clone(),
                at,
            })
            .await
            .map_err(|_| DaemonError::Internal("fleet task is gone".into()))?;
        let hook_event = HookEvent {
            agent: agent.to_string(),
            name: event.name,
            session_id: event.session_id,
            received_at: at,
            payload: event.payload,
        };
        tracing::debug!(agent = %agent, event = %hook_event.name, payload = %hook_event.payload, "hook event");
        let outcome = self.handler.handle(&hook_event).await;
        self.shared
            .metrics
            .hook_event(agent, &hook_event.name, started.elapsed().as_secs_f64());
        Ok(outcome)
    }
}

/// A tmux-mode daemon runs no pod (Spec O §20.3). The CLI's resolver
/// refuses this first; the daemon does not rely on that (#24).
fn refuse_pod_runner(spec: &FleetSpec) -> Result<(), DaemonError> {
    for (crew_name, crew) in &spec.crews {
        for (agent_name, settings) in &crew.agents {
            if settings.runner.kind() == balerix_api::RunnerKind::Pod {
                return Err(DaemonError::Invalid(format!(
                    "crews.{crew_name}.agents.{agent_name}.runner.type: `pod` runs only on \
                     Kubernetes; this daemon runs agents in tmux"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::PluginEventHandler;
    use crate::testing::{Harness, StubScript, ready_toolchain, stub_plugin, write_plugin_package};
    use balerix_api::{
        ActivationState, AgentPhase, AgentSettings, CrewSpec, GitSettings, PluginAction,
    };
    use balerix_core::fakes::FakeSystemToolchain;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::time::Duration;

    #[test]
    fn a_pod_runner_is_refused_with_its_config_path() {
        let spec: FleetSpec = serde_json::from_value(serde_json::json!({
            "name": "f",
            "crews": { "c": { "repo": "o/r", "ref": "main", "agents": {
                "a": {},
                "b": { "runner": { "type": "pod" } }
            } } }
        }))
        .unwrap();
        let DaemonError::Invalid(message) = refuse_pod_runner(&spec).unwrap_err() else {
            panic!("not Invalid")
        };
        assert_eq!(
            message,
            "crews.c.agents.b.runner.type: `pod` runs only on Kubernetes; this daemon runs agents in tmux"
        );
        let tmux_only: FleetSpec = serde_json::from_value(serde_json::json!({
            "name": "f",
            "crews": { "c": { "repo": "o/r", "ref": "main", "agents": { "a": {} } } }
        }))
        .unwrap();
        assert!(refuse_pod_runner(&tmux_only).is_ok());
    }

    fn spec(agents: &[(&str, &[(&str, serde_json::Value)])]) -> FleetSpec {
        FleetSpec {
            name: "f".into(),
            crews: BTreeMap::from([(
                "c".to_string(),
                CrewSpec {
                    repo: "acme/x".into(),
                    git_ref: "main".into(),
                    git: GitSettings::default(),
                    agents: agents
                        .iter()
                        .map(|(n, plugins)| {
                            let s = AgentSettings {
                                plugins: plugins
                                    .iter()
                                    .map(|(p, c)| (p.to_string(), c.clone()))
                                    .collect(),
                                ..Default::default()
                            };
                            (n.to_string(), s)
                        })
                        .collect(),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        }
    }

    struct World {
        h: Harness,
        daemon: Arc<Daemon>,
        _dir: tempfile::TempDir,
        dir: std::path::PathBuf,
    }

    /// A daemon with the chain handler and one declared plugin `flow`
    /// (intercepts PreToolUse and Stop, observes Stop, needs actions+kv)
    /// that has not said hello yet.
    async fn world() -> World {
        world_with(Vec::new()).await
    }

    /// The same, over stored records: what a daemon restart loads.
    async fn world_with(existing: Vec<(FleetRecord, FleetSecrets)>) -> World {
        world_full(existing, ready_toolchain()).await
    }

    /// The same, over a system toolchain the test controls: the pool actor
    /// `Daemon::start` spawns is the only thing that opens the fleets' gate.
    async fn world_with_toolchain(toolchain: Arc<dyn SystemToolchain>) -> World {
        world_full(Vec::new(), toolchain).await
    }

    async fn world_full(
        existing: Vec<(FleetRecord, FleetSecrets)>,
        toolchain: Arc<dyn SystemToolchain>,
    ) -> World {
        let h = Harness::new(Duration::from_secs(3600));
        let dir = tempfile::tempdir().unwrap();
        write_plugin_package(
            &dir.path().join("flow-pkg"),
            "flow",
            "hooks: { intercept: [PreToolUse, Stop], observe: [Stop] }\nneeds: [actions, kv]\n",
        );
        std::fs::write(
            dir.path().join("plugins.yaml"),
            "plugins:\n  - name: flow\n    source: ./flow-pkg\n",
        )
        .unwrap();
        let handler = PluginEventHandler::new(
            h.registry.clone(),
            h.client.clone(),
            Metrics::new().unwrap(),
        );
        let daemon = h.daemon_with_existing(handler, dir.path(), existing, toolchain);
        daemon.sync_plugins().await.unwrap();
        World {
            h,
            daemon,
            dir: dir.path().to_path_buf(),
            _dir: dir,
        }
    }

    async fn hello(w: &World, listen: &str) {
        let name: AgentName = "flow".parse().unwrap();
        let token = w.daemon.hook_secret(&plugin_id(&name)).await.unwrap();
        w.daemon
            .plugin_hello(
                &name,
                &token,
                HelloRequest {
                    name: "flow".into(),
                    version: "0.1.0".into(),
                    protocol: balerix_api::PLUGIN_PROTOCOL,
                    listen: listen.into(),
                    manifest: None,
                    revision: None,
                },
            )
            .await
            .unwrap();
    }

    /// The plugin agent's own `hello` reaches the actor behind its first
    /// pass; the phase in the record follows a moment later.
    async fn wait_plugin_ready(w: &World) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if w.daemon
                    .plugin_host()
                    .unwrap()
                    .list()
                    .await
                    .first()
                    .is_some_and(|p| p.phase == AgentPhase::Ready)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn wait_gen(daemon: &Daemon, g: u64) {
        let name: FleetName = "f".parse().unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if daemon
                    .get(&name)
                    .await
                    .is_some_and(|r| r.status.observed_generation == g)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn wait_down(daemon: &Daemon) {
        let name: FleetName = "f".parse().unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if daemon.get(&name).await.is_some_and(|r| r.is_down()) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    /// The Spec L §3.1 path, as the route calls it, for fleet `f`.
    async fn manage(
        w: &World,
        plugin: &str,
        file: serde_json::Value,
    ) -> Result<FleetRecord, DaemonError> {
        let name: FleetName = "f".parse().unwrap();
        w.daemon
            .manage_fleet(&plugin.parse().unwrap(), &name, file)
            .await
    }

    /// An unresolved fleet file; what the fake resolver answers is set by
    /// each test, so the crews here are only what the name check reads.
    fn file() -> serde_json::Value {
        json!({ "apiVersion": "balerix/v1", "kind": "Fleet", "name": "f", "crews": {} })
    }

    /// Polls to a bounded deadline. A bare sleep would be either flaky or
    /// slow, and naming what was expected keeps the failure readable.
    async fn eventually(what: &str, pred: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if pred() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    /// Spec F §4: the daemon pool has exactly one owner, spawned by
    /// `Daemon::start`, and it is what opens every fleet's gate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_spawns_the_system_pool_actor_and_fleets_wait_for_it() {
        // Fails its first attempt and the next one is a whole tick away, so
        // the pool stays unready for this test's lifetime: the daemon must
        // come up regardless (F-7), and fleets must not reconcile.
        let tc = Arc::new(FakeSystemToolchain::failing(1));
        let w = world_with_toolchain(tc.clone()).await;
        let name: FleetName = "f".parse().unwrap();

        // The API serves at once, whatever the pool is doing (F-3).
        let rec = w
            .daemon
            .apply(&name, spec(&[("a", &[])]), Default::default(), false)
            .await
            .unwrap();
        assert_eq!(rec.generation, 1);

        // Someone owns the pool: an actor is running and driving the port.
        eventually("the pool's first attempt", || tc.calls() >= 1).await;

        // And until it succeeds, no fleet pass materializes anything.
        assert!(
            w.h.materializer.calls().is_empty(),
            "fleets wait for the pool: {:?}",
            w.h.materializer.calls()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unknown_plugin_fails_the_apply_before_the_actor_sees_it() {
        let w = world().await;
        let name: FleetName = "f".parse().unwrap();
        let e = w
            .daemon
            .apply(
                &name,
                spec(&[("a", &[("nope", json!({}))])]),
                Default::default(),
                false,
            )
            .await
            .unwrap_err();
        assert_eq!(
            e,
            DaemonError::Invalid(
                "crews.c.agents.a.plugins.nope: no plugin \"nope\" is installed".into()
            )
        );
        assert!(w.daemon.get(&name).await.is_none(), "no actor was spawned");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pending_pair_is_activated_at_hello_and_a_rejection_is_recorded() {
        let w = world().await;
        let name: FleetName = "f".parse().unwrap();
        let rec = w
            .daemon
            .apply(
                &name,
                spec(&[
                    ("a", &[("flow", json!({ "v": 1 }))]),
                    ("b", &[("flow", json!({ "v": 2 }))]),
                ]),
                Default::default(),
                false,
            )
            .await
            .unwrap();
        wait_gen(&w.daemon, 1).await;
        let rec = w.daemon.get(&name).await.unwrap_or(rec);
        assert_eq!(
            rec.status.agents["f/c/a"].plugins["flow"].state,
            ActivationState::Pending
        );
        assert_eq!(
            rec.status.agents["f/c/b"].plugins["flow"].state,
            ActivationState::Pending
        );

        let stub = stub_plugin(StubScript {
            reject: BTreeMap::from([("f/c/b".to_string(), "states.x: unknown".to_string())]),
            health_ok: true,
            ..StubScript::default()
        })
        .await;
        hello(&w, &stub.listen).await;
        let rec = w.daemon.get(&name).await.unwrap();
        assert_eq!(
            rec.status.agents["f/c/a"].plugins["flow"].state,
            ActivationState::Active
        );
        let b = &rec.status.agents["f/c/b"].plugins["flow"];
        assert_eq!(
            (b.state, b.message.as_str()),
            (ActivationState::Rejected, "states.x: unknown")
        );
        let activates = stub.calls_named("activate");
        assert_eq!(activates.len(), 2);
        assert_eq!(activates[0]["agent"], "f/c/a");
        assert_eq!(activates[0]["config"]["v"], 1);
        assert_eq!(
            w.daemon.plugin_host().unwrap().list().await[0].active_agents,
            1
        );
        // a second hello re-activates everything again (restart recovery)
        hello(&w, &stub.listen).await;
        assert_eq!(stub.calls_named("activate").len(), 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_ready_plugin_is_activated_during_apply_and_a_rejection_fails_it() {
        let w = world().await;
        let stub = stub_plugin(StubScript {
            reject: BTreeMap::from([("f/c/bad".to_string(), "no".to_string())]),
            health_ok: true,
            ..StubScript::default()
        })
        .await;
        hello(&w, &stub.listen).await;
        let name: FleetName = "f".parse().unwrap();
        let e = w
            .daemon
            .apply(
                &name,
                spec(&[
                    ("a", &[("flow", json!({}))]),
                    ("bad", &[("flow", json!({}))]),
                ]),
                Default::default(),
                false,
            )
            .await
            .unwrap_err();
        assert_eq!(
            e,
            DaemonError::Invalid("crews.c.agents.bad.plugins.flow: no".into())
        );
        assert!(w.daemon.get(&name).await.is_none(), "nothing landed");
        assert_eq!(
            stub.calls_named("deactivate").len(),
            1,
            "the pair that had been activated is rolled back"
        );
        assert!(
            w.daemon
                .registry()
                .row(&"f/c/a".parse().unwrap(), &"flow".parse().unwrap())
                .is_none()
        );

        // a good spec: active at once; a changed config deactivates then activates;
        // a dropped agent deactivates; down deactivates the rest
        let rec = w
            .daemon
            .apply(
                &name,
                spec(&[
                    ("a", &[("flow", json!({ "v": 1 }))]),
                    ("c", &[("flow", json!({}))]),
                ]),
                Default::default(),
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            rec.status
                .agents
                .get("f/c/a")
                .map(|a| a.plugins["flow"].state),
            None,
            "the actor's first pass has not created the entry yet; the overlay skips unknown agents"
        );
        wait_gen(&w.daemon, 1).await;
        let rec = w.daemon.get(&name).await.unwrap();
        assert_eq!(
            rec.status.agents["f/c/a"].plugins["flow"].state,
            ActivationState::Active
        );
        let before = stub.calls().len();
        w.daemon
            .apply(
                &name,
                spec(&[("a", &[("flow", json!({ "v": 2 }))])]),
                Default::default(),
                true,
            )
            .await
            .unwrap();
        let after: Vec<String> = stub.calls()[before..]
            .iter()
            .map(|(r, v)| format!("{r} {}", v["agent"].as_str().unwrap_or_default()))
            .collect();
        assert_eq!(
            after,
            vec!["activate f/c/a", "deactivate f/c/c"],
            "a changed pair gets its new activate in place; only the dropped pair is deactivated"
        );

        // a rejection after a *changed* pair was already accepted puts that
        // pair back by re-activating the config its row still carries; no
        // deactivate is ever sent for a pair the new spec keeps
        let before = stub.calls().len();
        let e = w
            .daemon
            .apply(
                &name,
                spec(&[
                    ("a", &[("flow", json!({ "v": 3 }))]),
                    ("bad", &[("flow", json!({}))]),
                ]),
                Default::default(),
                true,
            )
            .await
            .unwrap_err();
        assert_eq!(
            e,
            DaemonError::Invalid("crews.c.agents.bad.plugins.flow: no".into())
        );
        let after: Vec<String> = stub.calls()[before..]
            .iter()
            .map(|(r, v)| {
                format!(
                    "{r} {} {}",
                    v["agent"].as_str().unwrap_or_default(),
                    v["config"]
                )
            })
            .collect();
        assert_eq!(
            after,
            vec![
                "activate f/c/a {\"v\":3}",
                "activate f/c/bad {}",
                "activate f/c/a {\"v\":2}",
            ],
            "the accepted changed pair is restored by re-activating its old config"
        );
        let row = w
            .daemon
            .registry()
            .row(&"f/c/a".parse().unwrap(), &"flow".parse().unwrap())
            .unwrap();
        assert_eq!(
            (row.activation.state, row.config),
            (ActivationState::Active, json!({ "v": 2 })),
            "the row never moved"
        );

        w.daemon.down(&name, Keep::default(), false).await.unwrap();
        let last = stub.calls().last().unwrap().clone();
        assert_eq!(
            (last.0.as_str(), last.1["agent"].as_str()),
            ("deactivate", Some("f/c/a"))
        );
        assert!(
            w.daemon
                .registry()
                .rows_for_plugin(&"flow".parse().unwrap())
                .is_empty()
        );
    }

    /// `down` answers before the actor's teardown pass, so an update
    /// landing in that window still sees the old spec in the record —
    /// the rows, which `down` cleared, are what an apply diffs against.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_update_right_after_down_activates_the_pairs_again() {
        let w = world().await;
        let stub = stub_plugin(StubScript {
            health_ok: true,
            ..StubScript::default()
        })
        .await;
        hello(&w, &stub.listen).await;
        let name: FleetName = "f".parse().unwrap();
        let s = spec(&[("a", &[("flow", json!({ "v": 1 }))])]);
        w.daemon
            .apply(&name, s.clone(), Default::default(), false)
            .await
            .unwrap();
        // the teardown pass fails, so the record never settles `Down` and
        // still names the old spec — the window the finding describes
        wait_gen(&w.daemon, 1).await;
        w.h.runner.fail_next("stop_agent", "f/c/a", "tmux is busy");
        w.daemon.down(&name, Keep::default(), false).await.unwrap();
        assert!(
            !w.daemon.get(&name).await.unwrap().is_down(),
            "the fleet has not settled down"
        );
        assert!(
            w.daemon.registry().rows_for_fleet(&name).is_empty(),
            "down dropped the rows"
        );
        let before = stub.calls_named("activate").len();
        w.daemon
            .apply(&name, s, Default::default(), true)
            .await
            .unwrap();
        assert_eq!(
            stub.calls_named("activate").len(),
            before + 1,
            "the pair is activated again, not diffed away"
        );
        wait_gen(&w.daemon, 2).await;
        assert_eq!(
            w.daemon.get(&name).await.unwrap().status.agents["f/c/a"].plugins["flow"].state,
            ActivationState::Active
        );
    }

    /// The rows are written before the actor hears of the spec (#113), so
    /// an apply the actor never takes has to put them back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_apply_the_actor_never_takes_leaves_the_rows_as_they_were() {
        let w = world().await;
        let stub = stub_plugin(StubScript {
            health_ok: true,
            ..StubScript::default()
        })
        .await;
        hello(&w, &stub.listen).await;
        let name: FleetName = "f".parse().unwrap();
        w.daemon
            .apply(
                &name,
                spec(&[("a", &[("flow", json!({ "v": 1 }))])]),
                Default::default(),
                false,
            )
            .await
            .unwrap();
        wait_gen(&w.daemon, 1).await;
        let before = w.daemon.registry().rows_for_fleet(&name);
        // a handle whose task drops every request unanswered (a closed
        // handle would be a purged fleet's, which counts as absent, #10)
        {
            let (tx, mut rx) = mpsc::channel::<Msg>(1);
            tokio::spawn(async move { while rx.recv().await.is_some() {} });
            let mut fleets = w.daemon.fleets.write().await;
            let status = fleets[&name].status.clone();
            fleets.insert(name.clone(), FleetHandle { tx, status });
        }
        let e = w
            .daemon
            .apply(
                &name,
                spec(&[
                    ("a", &[("flow", json!({ "v": 2 }))]),
                    ("b", &[("flow", json!({}))]),
                ]),
                Default::default(),
                true,
            )
            .await
            .unwrap_err();
        assert_eq!(
            e,
            DaemonError::Internal("fleet task dropped the request".into())
        );
        assert_eq!(
            w.daemon.registry().rows_for_fleet(&name),
            before,
            "the changed pair has its old row and the new pair has none"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn events_run_the_chain_and_actions_reach_the_runner_and_the_actor() {
        let w = world().await;
        let stub = stub_plugin(StubScript {
            verdict: json!({ "decision": "block", "reason": "no" }),
            actions: vec![
                PluginAction::SendText {
                    text: "fix it".into(),
                    submit: true,
                },
                PluginAction::Restart,
            ],
            health_ok: true,
            ..StubScript::default()
        })
        .await;
        hello(&w, &stub.listen).await;
        let name: FleetName = "f".parse().unwrap();
        w.daemon
            .apply(
                &name,
                spec(&[("a", &[("flow", json!({}))])]),
                Default::default(),
                false,
            )
            .await
            .unwrap();
        wait_gen(&w.daemon, 1).await;
        let agent: AgentId = "f/c/a".parse().unwrap();
        let secret = w.daemon.hook_secret(&agent).await.unwrap();
        let out = w
            .daemon
            .event(
                &agent,
                &secret,
                ParsedEvent {
                    name: "PreToolUse".into(),
                    session_id: None,
                    payload: json!({ "hook_event_name": "PreToolUse" }),
                },
            )
            .await
            .unwrap();
        assert_eq!(out.response, json!({ "decision": "block", "reason": "no" }));
        assert_eq!(out.actions.len(), 2);
        w.daemon
            .clone()
            .run_actions(agent.clone(), out.actions)
            .await;
        let calls = w.h.runner.calls();
        assert!(
            calls.contains(&"send_text f/c/a \"fix it\" submit=true".to_string()),
            "{calls:?}"
        );
        assert!(calls.contains(&"stop_agent f/c/a".to_string()));
        assert_eq!(
            calls.iter().filter(|c| *c == "ensure_agent f/c/a").count(),
            2,
            "restart = stop + start"
        );
        let rec = w.daemon.get(&name).await.unwrap();
        assert!(rec.stopped.is_empty(), "restart leaves nothing stopped");
        assert_eq!(rec.status.agents["f/c/a"].restarts, 0);
        w.daemon
            .execute_action(&agent, &PluginAction::Stop, Some("flow"))
            .await
            .unwrap();
        let rec = w.daemon.get(&name).await.unwrap();
        assert_eq!(rec.status.agents["f/c/a"].phase, AgentPhase::Stopped);
        assert!(rec.stopped.contains("f/c/a"));
        let text = w.daemon.metrics().encode();
        assert!(
            text.contains("balerix_plugin_actions_total{action=\"stop\",plugin=\"flow\"} 1"),
            "{text}"
        );
        assert!(text.contains(
            "balerix_hook_actions_total{action=\"restart\",agent=\"a\",crew=\"c\",fleet=\"f\"} 1"
        ));
        // an unknown event on a non-intercepting plugin: observers only
        w.daemon
            .event(
                &agent,
                &secret,
                ParsedEvent {
                    name: "Stop".into(),
                    session_id: None,
                    payload: json!({}),
                },
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while stub.calls_named("events").is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the observer batch arrived");
        assert_eq!(stub.calls_named("events")[0]["events"][0]["name"], "Stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_keys_reaches_the_runner_and_an_invalid_one_never_does() {
        use balerix_api::{Key, KeyStep};
        let w = world().await;
        let stub = stub_plugin(StubScript {
            health_ok: true,
            ..StubScript::default()
        })
        .await;
        hello(&w, &stub.listen).await;
        let name: FleetName = "f".parse().unwrap();
        w.daemon
            .apply(
                &name,
                spec(&[("a", &[("flow", json!({}))])]),
                Default::default(),
                false,
            )
            .await
            .unwrap();
        wait_gen(&w.daemon, 1).await;
        let agent: AgentId = "f/c/a".parse().unwrap();
        let ok = PluginAction::SendKeys {
            steps: vec![KeyStep::Key(Key::Down), KeyStep::Key(Key::Enter)],
            delay_ms: 20,
        };
        w.daemon
            .execute_action(&agent, &ok, Some("matrix"))
            .await
            .unwrap();
        assert!(
            w.h.runner
                .calls()
                .contains(&"send_keys f/c/a [down,enter] delay=20ms".to_string()),
            "{:?}",
            w.h.runner.calls()
        );

        let before = w.h.runner.calls().len();
        let too_fast = PluginAction::SendKeys {
            steps: vec![KeyStep::Key(Key::Enter)],
            delay_ms: 0,
        };
        let err = w
            .daemon
            .execute_action(&agent, &too_fast, Some("matrix"))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, DaemonError::Invalid(m) if m.starts_with("delay_ms:")),
            "{err:?}"
        );
        assert_eq!(w.h.runner.calls().len(), before, "never reached the runner");
    }

    /// A refused apply must not have told a plugin anything: the 409 and
    /// the 404 are decided before the activation block.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_conflict_or_a_missing_fleet_is_answered_before_any_plugin_call() {
        let w = world().await;
        let stub = stub_plugin(StubScript {
            health_ok: true,
            ..StubScript::default()
        })
        .await;
        hello(&w, &stub.listen).await;
        let name: FleetName = "f".parse().unwrap();
        w.daemon
            .apply(
                &name,
                spec(&[("a", &[("flow", json!({ "v": 1 }))])]),
                Default::default(),
                false,
            )
            .await
            .unwrap();
        wait_gen(&w.daemon, 1).await;
        let before = stub.calls().len();

        // POST on a live fleet, with a *changed* config: 409, and the
        // plugin still holds the config the fleet is running.
        let e = w
            .daemon
            .apply(
                &name,
                spec(&[("a", &[("flow", json!({ "v": 2 }))])]),
                Default::default(),
                false,
            )
            .await
            .unwrap_err();
        assert_eq!(e, DaemonError::Conflict);
        assert_eq!(
            stub.calls()[before..].len(),
            0,
            "no deactivate and no activate: {:?}",
            stub.calls()[before..].to_vec()
        );
        let row = w
            .daemon
            .registry()
            .row(&"f/c/a".parse().unwrap(), &"flow".parse().unwrap())
            .unwrap();
        assert_eq!(
            (row.activation.state, row.config),
            (ActivationState::Active, json!({ "v": 1 })),
            "the row still carries the config the plugin was given"
        );

        // PUT on an absent fleet: 404, and nothing was said either.
        let mut absent = spec(&[("a", &[("flow", json!({ "v": 9 }))])]);
        absent.name = "g".into();
        let e = w
            .daemon
            .apply(&"g".parse().unwrap(), absent, Default::default(), true)
            .await
            .unwrap_err();
        assert_eq!(e, DaemonError::NotFound);
        assert_eq!(stub.calls()[before..].len(), 0);
    }

    /// R24: an unchanged pair whose row is not `Active` is offered to the
    /// plugin again by the next apply — the fix for a plugin that was
    /// broken when the pair was first activated.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rejected_row_is_re_attempted_by_the_next_apply() {
        let w = world().await;
        let name: FleetName = "f".parse().unwrap();
        let flow: AgentName = "flow".parse().unwrap();
        let agent: AgentId = "f/c/a".parse().unwrap();
        let s = spec(&[("a", &[("flow", json!({ "v": 1 }))])]);
        w.daemon
            .apply(&name, s.clone(), Default::default(), false)
            .await
            .unwrap();
        wait_gen(&w.daemon, 1).await;
        assert_eq!(
            w.daemon
                .registry()
                .row(&agent, &flow)
                .unwrap()
                .activation
                .state,
            ActivationState::Pending,
            "the plugin was not ready"
        );

        let bad = stub_plugin(StubScript {
            reject: BTreeMap::from([("f/c/a".to_string(), "states.x: unknown".to_string())]),
            health_ok: true,
            ..StubScript::default()
        })
        .await;
        hello(&w, &bad.listen).await;
        assert_eq!(
            w.daemon
                .registry()
                .row(&agent, &flow)
                .unwrap()
                .activation
                .state,
            ActivationState::Rejected
        );

        // The plugin goes away (#11): an apply that cannot reach it keeps
        // the rejection, message and all, while the config is unchanged…
        // (`unhello`, not `set_ready(false)`: the readiness watcher sets
        // `ready` again on every record that shows the plugin agent
        // `Ready`, and its `hello` above may still be on its way there.)
        w.daemon.registry().unhello(&flow);
        w.daemon
            .apply(&name, s.clone(), Default::default(), true)
            .await
            .unwrap();
        assert_eq!(
            w.daemon.registry().row(&agent, &flow).unwrap().activation,
            PluginActivation::rejected("states.x: unknown"),
            "a not-ready plugin leaves an unchanged rejected row alone"
        );
        // …and a changed config is pending again, still carrying the last
        // rejection for `status` until a plugin answers.
        let s2 = spec(&[("a", &[("flow", json!({ "v": 2 }))])]);
        w.daemon
            .apply(&name, s2.clone(), Default::default(), true)
            .await
            .unwrap();
        let row = w.daemon.registry().row(&agent, &flow).unwrap();
        assert_eq!(row.config, json!({ "v": 2 }));
        assert_eq!(
            row.activation,
            PluginActivation {
                state: ActivationState::Pending,
                message: "states.x: unknown".into(),
            }
        );
        // An unchanged pending row is not reset by another apply either.
        w.daemon
            .apply(&name, s2.clone(), Default::default(), true)
            .await
            .unwrap();
        assert_eq!(
            w.daemon
                .registry()
                .row(&agent, &flow)
                .unwrap()
                .activation
                .message,
            "states.x: unknown"
        );

        // The plugin is fixed and listening again. No `hello` here: the
        // apply itself must re-attempt the row, whose config is unchanged.
        let good = stub_plugin(StubScript {
            health_ok: true,
            ..StubScript::default()
        })
        .await;
        w.daemon
            .registry()
            .set_listen(&flow, good.listen.clone(), "t".into());
        w.daemon
            .apply(&name, s2, Default::default(), true)
            .await
            .unwrap();
        assert_eq!(
            good.calls_named("activate").len(),
            1,
            "the unchanged pair was activated again"
        );
        assert_eq!(good.calls_named("activate")[0]["agent"], "f/c/a");
        assert!(
            good.calls_named("deactivate").is_empty(),
            "a re-attempt is not a config change"
        );
        assert_eq!(
            w.daemon.registry().row(&agent, &flow).unwrap().activation,
            PluginActivation::active(),
            "an accepted pair clears the old rejection"
        );
    }

    /// `Daemon::start` rebuilds the pending rows of every stored `Up`
    /// record; a stored `Down` record has none (§16.2).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stored_up_record_starts_its_pairs_pending() {
        let up = FleetRecord::new(spec(&[("a", &[("flow", json!({ "v": 1 }))])]));
        let mut down_spec = spec(&[("a", &[("flow", json!({ "v": 2 }))])]);
        down_spec.name = "g".into();
        let mut down = FleetRecord::new(down_spec);
        down.desired = Desired::Down {
            keep: Keep::default(),
            purge: false,
        };
        let w = world_with(vec![
            (up, FleetSecrets::default()),
            (down, FleetSecrets::default()),
        ])
        .await;
        let flow: AgentName = "flow".parse().unwrap();
        let agent: AgentId = "f/c/a".parse().unwrap();
        let row = w.daemon.registry().row(&agent, &flow).unwrap();
        assert_eq!(
            (row.activation.state, row.config),
            (ActivationState::Pending, json!({ "v": 1 })),
            "the stored fleet's pair is pending: nothing about activation is persisted"
        );
        assert!(
            w.daemon
                .registry()
                .rows_for_fleet(&"g".parse().unwrap())
                .is_empty(),
            "a stored `Down` record gets no rows"
        );

        let stub = stub_plugin(StubScript {
            health_ok: true,
            ..StubScript::default()
        })
        .await;
        hello(&w, &stub.listen).await;
        assert_eq!(
            w.daemon
                .registry()
                .row(&agent, &flow)
                .unwrap()
                .activation
                .state,
            ActivationState::Active
        );
        let activates = stub.calls_named("activate");
        assert_eq!(activates.len(), 1);
        assert_eq!(activates[0]["agent"], "f/c/a");
        assert_eq!(activates[0]["config"]["v"], 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokens_identify_plugins_and_the_health_poller_marks_degraded() {
        let w = world().await;
        let name: AgentName = "flow".parse().unwrap();
        let token = w.daemon.hook_secret(&plugin_id(&name)).await.unwrap();
        assert_eq!(w.daemon.plugin_for_token(&token).await, Some(name.clone()));
        assert_eq!(w.daemon.plugin_for_token("nope").await, None);
        let stub = stub_plugin(StubScript {
            health_ok: false,
            ..StubScript::default()
        })
        .await;
        hello(&w, &stub.listen).await;
        wait_plugin_ready(&w).await;
        w.daemon.poll_health().await;
        let rows = w.daemon.plugin_host().unwrap().list().await;
        assert_eq!(rows[0].message, "degraded: HTTP 503");
        assert_eq!(rows[0].phase, AgentPhase::Ready, "never restarted for it");
        hello(&w, &stub.listen).await;
        assert_eq!(
            w.daemon.plugin_host().unwrap().list().await[0].message,
            "",
            "hello clears it"
        );
        let _ = &w.dir;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_plugin_applies_a_fleet_from_a_file_and_owns_it() {
        let w = world().await;
        let name: FleetName = "f".parse().unwrap();
        w.h.resolver.set(Ok(spec(&[("a", &[])])));
        let rec = manage(&w, "flow", file()).await.unwrap();
        assert_eq!(rec.owner.as_deref(), Some("flow"));
        assert_eq!(rec.generation, 1);
        assert_eq!(rec.spec.name, "f");
        assert_eq!(w.h.resolver.calls(), vec![("f".to_string(), file())]);
        assert_eq!(
            w.h.credentials.calls(),
            1,
            "the operator's bundle is read at apply time"
        );
        wait_gen(&w.daemon, 1).await;
        assert!(
            w.h.materializer.calls().iter().any(|c| c.contains("f/c/a")),
            "the fleet runs: {:?}",
            w.h.materializer.calls()
        );

        // a second apply is an upsert: replaced in place
        let rec = manage(&w, "flow", file()).await.unwrap();
        assert_eq!((rec.generation, rec.owner.as_deref()), (2, Some("flow")));

        // the admin routes refuse it, and so does another plugin
        let managed = DaemonError::Managed("fleet f is managed by plugin flow".into());
        assert_eq!(
            w.daemon
                .apply(&name, spec(&[("a", &[])]), Default::default(), false)
                .await
                .unwrap_err(),
            managed
        );
        assert_eq!(
            w.daemon
                .apply(&name, spec(&[("a", &[])]), Default::default(), true)
                .await
                .unwrap_err(),
            managed
        );
        assert_eq!(
            w.daemon
                .down(&name, Keep::default(), false)
                .await
                .unwrap_err(),
            managed
        );
        assert_eq!(manage(&w, "web", file()).await.unwrap_err(), managed);
        let web = Caller::Plugin("web".parse().unwrap());
        assert_eq!(
            w.daemon
                .down_as(&name, Keep::default(), false, &web)
                .await
                .unwrap_err(),
            managed
        );
        assert_eq!(
            w.h.resolver.calls().len(),
            2,
            "a foreign plugin's apply is refused before resolving"
        );

        // the owner downs it; the owner stays; the owner's next apply resumes it
        let flow = Caller::Plugin("flow".parse().unwrap());
        let rec = w
            .daemon
            .down_as(&name, Keep::default(), false, &flow)
            .await
            .unwrap();
        assert!(matches!(rec.desired, Desired::Down { .. }));
        assert_eq!(rec.owner.as_deref(), Some("flow"));
        assert_eq!(w.daemon.list().await[0].managed_by.as_deref(), Some("flow"));
        wait_down(&w.daemon).await;
        let rec = manage(&w, "flow", file()).await.unwrap();
        assert_eq!((rec.generation, rec.desired), (3, Desired::Up));
        // and the admin can force it down
        let rec = w
            .daemon
            .down_as(
                &name,
                Keep::default(),
                false,
                &Caller::Admin { force: true },
            )
            .await
            .unwrap();
        assert!(matches!(rec.desired, Desired::Down { .. }));
        assert_eq!(
            rec.owner.as_deref(),
            Some("flow"),
            "a forced down keeps the owner"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_plugin_cannot_take_a_fleet_the_cli_created() {
        let w = world().await;
        let name: FleetName = "f".parse().unwrap();
        w.daemon
            .apply(&name, spec(&[("a", &[])]), Default::default(), false)
            .await
            .unwrap();
        w.h.resolver.set(Ok(spec(&[("a", &[])])));
        let not_managed = DaemonError::Managed("fleet f is not managed by a plugin".into());
        assert_eq!(manage(&w, "flow", file()).await.unwrap_err(), not_managed);
        let flow = Caller::Plugin("flow".parse().unwrap());
        assert_eq!(
            w.daemon
                .down_as(&name, Keep::default(), false, &flow)
                .await
                .unwrap_err(),
            not_managed
        );
        assert!(w.h.resolver.calls().is_empty(), "refused before resolving");
        // even once it is down: ownership is never transferred (Spec L §9)
        w.daemon.down(&name, Keep::default(), false).await.unwrap();
        wait_down(&w.daemon).await;
        assert_eq!(manage(&w, "flow", file()).await.unwrap_err(), not_managed);
        // while the CLI still may re-apply it in place
        assert_eq!(
            w.daemon
                .apply(&name, spec(&[("a", &[])]), Default::default(), false)
                .await
                .unwrap()
                .generation,
            2
        );
    }

    /// Review focus 2.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_file_whose_name_disagrees_with_the_path_is_refused_before_resolving() {
        let w = world().await;
        let mut f = file();
        f["name"] = json!("g");
        assert_eq!(
            manage(&w, "flow", f).await.unwrap_err(),
            DaemonError::Invalid("name: \"g\" does not match the fleet f".into())
        );
        assert!(w.h.resolver.calls().is_empty());
        assert_eq!(w.h.credentials.calls(), 0);
        // a file without a name resolves under the path's name
        let mut f = file();
        f.as_object_mut().unwrap().remove("name");
        w.h.resolver.set(Ok(spec(&[("a", &[])])));
        assert_eq!(manage(&w, "flow", f).await.unwrap().spec.name, "f");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_resolver_or_credential_failure_lands_nothing() {
        let w = world().await;
        let name: FleetName = "f".parse().unwrap();
        let bad_version = "crews.c.agents.a.tools.node: expected an exact version, got \"22\" (try: mise latest node@22)";
        w.h.resolver.set(Err(bad_version.into()));
        assert_eq!(
            manage(&w, "flow", file()).await.unwrap_err(),
            DaemonError::Invalid(bad_version.into())
        );
        assert!(w.daemon.get(&name).await.is_none(), "no actor was spawned");
        assert_eq!(
            w.h.credentials.calls(),
            0,
            "credentials are read only after a successful resolve"
        );
        w.h.resolver.set(Ok(spec(&[("a", &[])])));
        w.h.credentials
            .set(Err("/home/op/.claude/settings.json: invalid JSON".into()));
        assert_eq!(
            manage(&w, "flow", file()).await.unwrap_err(),
            DaemonError::Internal("/home/op/.claude/settings.json: invalid JSON".into())
        );
        assert!(w.daemon.get(&name).await.is_none());
        // a rejected activation is still a 400 with nothing landed: the
        // apply's existing rule holds for a plugin's apply too
        let stub = stub_plugin(StubScript {
            reject: BTreeMap::from([("f/c/bad".to_string(), "no".to_string())]),
            health_ok: true,
            ..StubScript::default()
        })
        .await;
        hello(&w, &stub.listen).await;
        w.h.credentials.set(Ok(Default::default()));
        w.h.resolver
            .set(Ok(spec(&[("bad", &[("flow", json!({}))])])));
        assert_eq!(
            manage(&w, "flow", file()).await.unwrap_err(),
            DaemonError::Invalid("crews.c.agents.bad.plugins.flow: no".into())
        );
        assert!(w.daemon.get(&name).await.is_none());
    }

    /// Review focus 5.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reserved_names_are_refused_by_the_manage_path() {
        let w = world().await;
        w.h.resolver.set(Ok(spec(&[("a", &[])])));
        for reserved in ["balerix", "watch"] {
            let name: FleetName = reserved.parse().unwrap();
            let e = w
                .daemon
                .manage_fleet(
                    &"flow".parse().unwrap(),
                    &name,
                    json!({ "apiVersion": "balerix/v1", "kind": "Fleet", "crews": {} }),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(&e, DaemonError::Invalid(m) if m.starts_with("name:")),
                "{reserved}: {e}"
            );
        }
        assert!(w.h.resolver.calls().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn removing_a_plugin_downs_the_fleets_it_owns() {
        let w = world().await;
        let flow: AgentName = "flow".parse().unwrap();
        w.h.resolver.set(Ok(spec(&[("a", &[])])));
        manage(&w, "flow", file()).await.unwrap();
        // beside it: a CLI fleet, and an owned fleet already going down
        let g: FleetName = "g".parse().unwrap();
        let mut g_spec = spec(&[("a", &[])]);
        g_spec.name = "g".into();
        w.daemon
            .apply(&g, g_spec, Default::default(), false)
            .await
            .unwrap();
        let h: FleetName = "h".parse().unwrap();
        let nameless = json!({ "apiVersion": "balerix/v1", "kind": "Fleet", "crews": {} });
        w.daemon.manage_fleet(&flow, &h, nameless).await.unwrap();
        w.daemon
            .down_as(&h, Keep::default(), false, &Caller::Plugin(flow.clone()))
            .await
            .unwrap();

        std::fs::write(w.dir.join("plugins.yaml"), "plugins: []\n").unwrap();
        let report = w.daemon.sync_plugins().await.unwrap();
        assert_eq!(report.stopped, vec!["flow".to_string()]);
        assert_eq!(
            report.downed,
            vec!["f".to_string()],
            "only the owned fleet that was up"
        );
        assert!(report.down_failed.is_empty());
        let f = w.daemon.get(&"f".parse().unwrap()).await.unwrap();
        assert!(matches!(f.desired, Desired::Down { .. }));
        assert_eq!(
            f.owner.as_deref(),
            Some("flow"),
            "the owner is kept through the down"
        );
        assert_eq!(
            w.daemon.get(&g).await.unwrap().desired,
            Desired::Up,
            "the CLI's fleet is untouched"
        );
    }

    /// Spec L-6 (F2): a plugin removed from `plugins.yaml` while the
    /// daemon was stopped is never "stopped" by a sync, so the first sync
    /// at start downs its fleets by the declared set. A fleet whose owner
    /// is declared is left alone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_first_sync_downs_the_fleets_of_a_plugin_no_longer_declared() {
        let orphan = FleetRecord::with_owner(spec(&[("a", &[])]), Some("gone".into()));
        let mut kept_spec = spec(&[("a", &[])]);
        kept_spec.name = "k".into();
        let kept = FleetRecord::with_owner(kept_spec, Some("flow".into()));
        // `world_with` declares `flow` only and runs the first sync
        let w = world_with(vec![
            (orphan, FleetSecrets::default()),
            (kept, FleetSecrets::default()),
        ])
        .await;
        let f = w.daemon.get(&"f".parse().unwrap()).await.unwrap();
        assert!(matches!(f.desired, Desired::Down { .. }), "{:?}", f.desired);
        assert_eq!(f.owner.as_deref(), Some("gone"), "the owner is kept");
        assert_eq!(
            w.daemon.get(&"k".parse().unwrap()).await.unwrap().desired,
            Desired::Up,
            "a declared plugin's fleet is untouched"
        );
        // a later sync finds nothing more to down
        let report = w.daemon.sync_plugins().await.unwrap();
        assert!(report.downed.is_empty(), "{:?}", report.downed);
        assert!(report.down_failed.is_empty());
    }

    /// Review focus 4.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stored_owned_record_keeps_its_owner_after_a_restart() {
        let stored = FleetRecord::with_owner(spec(&[("a", &[])]), Some("flow".into()));
        let w = world_with(vec![(stored, FleetSecrets::default())]).await;
        let name: FleetName = "f".parse().unwrap();
        assert_eq!(
            w.daemon.get(&name).await.unwrap().owner.as_deref(),
            Some("flow")
        );
        assert_eq!(w.daemon.list().await[0].managed_by.as_deref(), Some("flow"));
        let managed = DaemonError::Managed("fleet f is managed by plugin flow".into());
        assert_eq!(
            w.daemon
                .apply(&name, spec(&[("a", &[])]), Default::default(), true)
                .await
                .unwrap_err(),
            managed
        );
        assert_eq!(
            w.daemon
                .down(&name, Keep::default(), false)
                .await
                .unwrap_err(),
            managed
        );
        // the plugin resumes it; the admin can force it down
        w.h.resolver.set(Ok(spec(&[("a", &[])])));
        assert_eq!(manage(&w, "flow", file()).await.unwrap().generation, 1);
        w.daemon
            .down_as(
                &name,
                Keep::default(),
                false,
                &Caller::Admin { force: true },
            )
            .await
            .unwrap();
    }

    /// #62: a plugin `PUT` in flight across the plugin's removal must not
    /// re-raise a fleet the sync downed, or create one the sync's snapshot
    /// never saw. `apply_as` refuses a plugin caller the registry no
    /// longer lists, under the fleet's lock, so the refusal is ordered
    /// against the sync's `replace_plugins`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_removed_plugin_cannot_apply_a_fleet() {
        let w = world().await;
        let flow: AgentName = "flow".parse().unwrap();
        w.h.resolver.set(Ok(spec(&[("a", &[])])));
        manage(&w, "flow", file()).await.unwrap();
        std::fs::write(w.dir.join("plugins.yaml"), "plugins: []\n").unwrap();
        let report = w.daemon.sync_plugins().await.unwrap();
        assert_eq!(report.downed, vec!["f".to_string()]);

        // the fleet the sync downed: not re-raised, and never resolved
        // without the plugin's `fleetDefaults` (fail closed, not `{}`)
        let resolved = w.h.resolver.layers().len();
        let refused = DaemonError::Managed("fleet f: plugin flow is not installed".into());
        assert_eq!(manage(&w, "flow", file()).await.unwrap_err(), refused);
        assert_eq!(
            w.h.resolver.layers().len(),
            resolved,
            "an unlisted plugin's file never reaches the resolver"
        );
        let f = w.daemon.get(&"f".parse().unwrap()).await.unwrap();
        assert!(matches!(f.desired, Desired::Down { .. }), "{:?}", f.desired);
        assert_eq!(f.owner.as_deref(), Some("flow"));

        // a new name: not created
        let h: FleetName = "h".parse().unwrap();
        let nameless = json!({ "apiVersion": "balerix/v1", "kind": "Fleet", "crews": {} });
        assert_eq!(
            w.daemon
                .manage_fleet(&flow, &h, nameless)
                .await
                .unwrap_err(),
            DaemonError::Managed("fleet h: plugin flow is not installed".into())
        );
        assert!(w.daemon.get(&h).await.is_none(), "no record was created");
    }

    /// A handle for fleet `f` whose actor has purged (or is purging) the
    /// fleet: its record says `Down` with `purge`, settled. With `rx`
    /// dropped the actor is gone; held, it is still on its way out.
    fn purging_handle() -> (FleetHandle, mpsc::Receiver<Msg>) {
        let mut record = FleetRecord::new(spec(&[("a", &[])]));
        record.desired = Desired::Down {
            keep: Keep::default(),
            purge: true,
        };
        record.status.phase = balerix_api::FleetPhase::Down;
        let (tx, rx) = mpsc::channel(1);
        let (_publish, status) = watch::channel(record);
        (FleetHandle { tx, status }, rx)
    }

    /// #10: an apply that got the fleet's lock after a `down --purge`
    /// finds the purged actor's handle still in the map (the purge
    /// listener has not run yet). It must start the fleet afresh, and the
    /// listener, running late, must not delete the fleet it made.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_apply_after_a_purge_is_not_undone_by_the_late_forget() {
        let w = world().await;
        let name: FleetName = "f".parse().unwrap();
        let (dead, rx) = purging_handle();
        drop(rx);
        w.daemon.fleets.write().await.insert(name.clone(), dead);

        let rec = w
            .daemon
            .apply(&name, spec(&[("a", &[])]), Default::default(), false)
            .await
            .unwrap();
        assert_eq!(rec.generation, 1, "a new fleet, not the purged one");
        wait_gen(&w.daemon, 1).await;

        w.daemon.forget_one(&name).await;
        let live = w.daemon.fleets.read().await.get(&name).cloned().unwrap();
        assert!(!live.tx.is_closed(), "the new actor is the one in the map");
        assert!(!w.daemon.get(&name).await.unwrap().is_down());

        // And a forget of a fleet whose actor really is gone removes it.
        let (dead, rx) = purging_handle();
        drop(rx);
        let g: FleetName = "g".parse().unwrap();
        w.daemon.fleets.write().await.insert(g.clone(), dead);
        w.daemon.forget_one(&g).await;
        assert!(w.daemon.get(&g).await.is_none());
    }

    /// #10: the purge is decided but the actor has not ended yet — an
    /// apply waits for it rather than handing the spec to a task that is
    /// about to drop it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_apply_waits_for_a_purging_actor_to_end() {
        let w = world().await;
        let name: FleetName = "f".parse().unwrap();
        let (purging, rx) = purging_handle();
        w.daemon.fleets.write().await.insert(name.clone(), purging);

        let d = w.daemon.clone();
        let n = name.clone();
        let apply = tokio::spawn(async move {
            d.apply(&n, spec(&[("a", &[])]), Default::default(), false)
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!apply.is_finished(), "waits for the actor to end");
        drop(rx);
        let rec = tokio::time::timeout(Duration::from_secs(5), apply)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(rec.generation, 1);
        wait_gen(&w.daemon, 1).await;
    }

    /// A plugin that holds its answer to the *first* `activate` matching
    /// `gated` until `open` is notified, then answers it (400 when
    /// `reject_gated`, else 200); every other `activate` is accepted at
    /// once. `seen` is every `(agent, config)` in arrival order,
    /// `answered` in the order the plugin took them (what it holds last).
    #[derive(Clone)]
    struct Gate {
        seen: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
        answered: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
        open: Arc<tokio::sync::Notify>,
        armed: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Gate {
        fn arrived(&self, agent: &str, config: &serde_json::Value) -> bool {
            self.seen
                .lock()
                .unwrap()
                .contains(&(agent.to_string(), config.clone()))
        }
        fn last_for(&self, agent: &str) -> Option<serde_json::Value> {
            self.answered
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(a, _)| a == agent)
                .map(|(_, c)| c.clone())
        }
    }

    async fn gated_plugin(
        gated: (&'static str, serde_json::Value),
        reject_gated: bool,
    ) -> (Gate, String) {
        use axum::extract::State;
        use axum::routing::{get, post};
        let gate = Gate {
            seen: Arc::default(),
            answered: Arc::default(),
            open: Arc::default(),
            armed: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let app = axum::Router::new()
            .route(
                "/v1/activate",
                post(
                    move |State(g): State<Gate>, axum::Json(req): axum::Json<serde_json::Value>| {
                        let gated = gated.clone();
                        async move {
                            let agent = req["agent"].as_str().unwrap_or_default().to_string();
                            g.seen
                                .lock()
                                .unwrap()
                                .push((agent.clone(), req["config"].clone()));
                            if agent == gated.0
                                && req["config"] == gated.1
                                && g.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
                            {
                                g.open.notified().await;
                                if reject_gated {
                                    // a refusal leaves the plugin as it was
                                    return (
                                        axum::http::StatusCode::BAD_REQUEST,
                                        axum::Json(json!({ "error": "stale" })),
                                    );
                                }
                            }
                            g.answered
                                .lock()
                                .unwrap()
                                .push((agent, req["config"].clone()));
                            (axum::http::StatusCode::OK, axum::Json(json!({})))
                        }
                    },
                ),
            )
            .route("/v1/health", get(|| async { axum::Json(json!({})) }))
            .with_state(gate.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move { axum::serve(listener, app).await });
        (gate, listen)
    }

    fn spawn_hello(w: &World, listen: String) -> tokio::task::JoinHandle<()> {
        let d = w.daemon.clone();
        tokio::spawn(async move {
            let flow: AgentName = "flow".parse().unwrap();
            let token = d.hook_secret(&plugin_id(&flow)).await.unwrap();
            d.plugin_hello(
                &flow,
                &token,
                HelloRequest {
                    name: "flow".into(),
                    version: "0.1.0".into(),
                    protocol: balerix_api::PLUGIN_PROTOCOL,
                    listen,
                    manifest: None,
                    revision: None,
                },
            )
            .await
            .unwrap();
        })
    }

    /// #10: `hello`'s re-activation runs outside the fleet's lock (a
    /// plugin whose `activate` calls back into `PUT fleets/{f}` would wait
    /// on it). An apply that changes the row while hello's `activate` is
    /// in flight wins: the stale answer is not written onto its row, and
    /// the plugin is offered the row's current config again, so it does
    /// not end up holding the old one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hello_answer_does_not_clobber_a_concurrent_apply() {
        let w = world().await;
        let name: FleetName = "f".parse().unwrap();
        let flow: AgentName = "flow".parse().unwrap();
        let agent: AgentId = "f/c/a".parse().unwrap();
        w.daemon
            .apply(
                &name,
                spec(&[("a", &[("flow", json!({ "v": 1 }))])]),
                Default::default(),
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            w.daemon.registry().row(&agent, &flow).unwrap().activation,
            PluginActivation::pending()
        );
        for reject_gated in [true, false] {
            // back to v1, pending, and a plugin not yet listening
            w.daemon.registry().unhello(&flow);
            w.daemon
                .apply(
                    &name,
                    spec(&[("a", &[("flow", json!({ "v": 1 }))])]),
                    Default::default(),
                    true,
                )
                .await
                .unwrap();
            let (gate, listen) = gated_plugin(("f/c/a", json!({ "v": 1 })), reject_gated).await;
            let said_hello = spawn_hello(&w, listen);
            eventually("hello's activate of v1 is in flight", || {
                gate.arrived("f/c/a", &json!({ "v": 1 }))
            })
            .await;
            w.daemon
                .apply(
                    &name,
                    spec(&[("a", &[("flow", json!({ "v": 2 }))])]),
                    Default::default(),
                    true,
                )
                .await
                .unwrap();
            assert_eq!(gate.last_for("f/c/a"), Some(json!({ "v": 2 })));
            gate.open.notify_one();
            tokio::time::timeout(Duration::from_secs(5), said_hello)
                .await
                .unwrap()
                .unwrap();
            let row = w.daemon.registry().row(&agent, &flow).unwrap();
            assert_eq!(
                (row.config, row.activation),
                (json!({ "v": 2 }), PluginActivation::active()),
                "reject_gated={reject_gated}: the answer about v1 is not written onto v2's row"
            );
            assert_eq!(
                gate.last_for("f/c/a"),
                Some(json!({ "v": 2 })),
                "reject_gated={reject_gated}: the plugin holds the row's config"
            );
        }
    }

    /// #10: hello re-activates its rows one after another; a row an apply
    /// moved on while an earlier row's `activate` was in flight is offered
    /// with its config *now*, not the one the loop started with.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hello_offers_each_row_its_current_config() {
        let w = world().await;
        let name: FleetName = "f".parse().unwrap();
        let flow: AgentName = "flow".parse().unwrap();
        w.daemon
            .apply(
                &name,
                spec(&[
                    ("a", &[("flow", json!({ "a": 1 }))]),
                    ("b", &[("flow", json!({ "b": 1 }))]),
                ]),
                Default::default(),
                false,
            )
            .await
            .unwrap();
        let (gate, listen) = gated_plugin(("f/c/a", json!({ "a": 1 })), false).await;
        let said_hello = spawn_hello(&w, listen);
        eventually("hello's activate of a is in flight", || {
            gate.arrived("f/c/a", &json!({ "a": 1 }))
        })
        .await;
        w.daemon
            .apply(
                &name,
                spec(&[
                    ("a", &[("flow", json!({ "a": 1 }))]),
                    ("b", &[("flow", json!({ "b": 2 }))]),
                ]),
                Default::default(),
                true,
            )
            .await
            .unwrap();
        gate.open.notify_one();
        tokio::time::timeout(Duration::from_secs(5), said_hello)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(gate.last_for("f/c/b"), Some(json!({ "b": 2 })));
        assert!(
            !gate
                .seen
                .lock()
                .unwrap()
                .contains(&("f/c/b".to_string(), json!({ "b": 1 }))),
            "b's old config was never offered: {:?}",
            gate.seen.lock().unwrap()
        );
        let row = w
            .daemon
            .registry()
            .row(&"f/c/b".parse().unwrap(), &flow)
            .unwrap();
        assert_eq!(
            (row.config, row.activation),
            (json!({ "b": 2 }), PluginActivation::active())
        );
    }
}
