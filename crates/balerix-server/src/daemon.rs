//! The registry (Phase 3 spec §3.1): fleet name → actor handle, the shared
//! secret index, and the request-side logic the API calls into.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use balerix_api::{
    ActivateRequest, ActivationState, CredentialBundle, DeactivateRequest, Desired, FleetSpec,
    FleetSummary, HelloRequest, HelloResponse, HookEvent, PluginAction, PluginActivation,
    SyncReport,
};
use balerix_core::{
    AgentId, AgentName, AgentRunner, EventHandler, Fleet, FleetName, FleetRecord, FleetSecrets,
    Keep, Outcome, SystemToolchain, WorkspaceReader, is_reserved_fleet, plugin_id,
    reserved_fleet_reason,
};
use tokio::sync::{RwLock, mpsc, oneshot, watch};

use crate::actor::{self, FleetHandle, Msg, Ports, Shared};
use crate::auth::constant_time_eq;
use crate::hooks::ParsedEvent;
use crate::metrics::Metrics;
use crate::plugins::activation::{self, Pair};
use crate::plugins::{
    ActivationRow, CallFailure, PluginClient, PluginError, PluginHost, PluginHostConfig, PluginKv,
    PluginRegistry,
};
use crate::sessions::Sessions;
use crate::system_pool::SystemPoolConfig;

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

/// How often every ready plugin's `GET /v1/health` is polled (§16.5).
pub const HEALTH_INTERVAL: Duration = Duration::from_secs(10);

pub struct Daemon {
    fleets: RwLock<BTreeMap<FleetName, FleetHandle>>,
    ports: Arc<Ports>,
    shared: Shared,
    handler: Arc<dyn DaemonHandler>,
    token: String,
    plugins: Arc<PluginHost>,
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
        plugin_config: PluginHostConfig,
        registry: Arc<PluginRegistry>,
        client: PluginClient,
        kv: Arc<PluginKv>,
        system_toolchain: Arc<dyn SystemToolchain>,
    ) -> Arc<Self> {
        let (shared, purged, pool_tx) = actor::shared(metrics);
        // Spec F §4: one owner for the daemon pool. Spawned before the fleet
        // actors, though they gate on readiness rather than on spawn order.
        crate::system_pool::spawn(system_toolchain, pool_tx, SystemPoolConfig::default());
        let plugins = PluginHost::start(plugin_config, &ports, shared.clone(), registry.clone());
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
        let daemon = Arc::new(Self {
            fleets: RwLock::new(fleets),
            ports,
            shared,
            handler,
            token,
            plugins,
            registry,
            client,
            proxy_client: crate::proxy::client(),
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
    pub async fn poll_health(&self) {
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
            d.fleets.write().await.remove(&name);
            d.bump();
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn metrics(&self) -> &Metrics {
        &self.shared.metrics
    }

    pub fn plugins(&self) -> &Arc<PluginHost> {
        &self.plugins
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

    /// `origin/<ref>` of the agent's crew, from the fleet's record; `None`
    /// when the fleet or the crew is unknown.
    pub async fn base_ref(&self, agent: &AgentId) -> Option<String> {
        let record = self.get(&agent.fleet).await?;
        let crew = record.spec.crews.get(agent.crew.as_str())?;
        Some(format!("origin/{}", crew.git_ref))
    }

    /// Reconciles the plugin set to `plugins.yaml`; `serve` calls it once
    /// at start and fails fast on an error, `plugin sync` on demand.
    pub async fn sync_plugins(&self) -> Result<SyncReport, PluginError> {
        let report = self.plugins.sync().await;
        // `replace_plugins` drops the activation rows of every plugin the
        // sync removed, and the plugin fleet's own actor snapshot is not
        // forwarded to `changes` — so this registry write ticks like the
        // others, or `fleets/watch` keeps serving the removed rows. A
        // sync that fails resolving does so before it replaces anything;
        // the failures after `replace_plugins` are the actor being gone,
        // which is shutdown, when no watcher is left to tell.
        if report.is_ok() {
            self.bump();
        }
        report
    }

    /// `hello` authenticates with the plugin's token — the hook secret the
    /// actor minted for `balerix/plugins/<name>` — and is otherwise the
    /// plugin's `SessionStart`.
    pub async fn plugin_hello(
        &self,
        name: &AgentName,
        token: &str,
        req: HelloRequest,
    ) -> Result<HelloResponse, DaemonError> {
        if !self.verify_secret(&plugin_id(name), token).await {
            return Err(DaemonError::Unauthorized);
        }
        let response = self.plugins.hello(name, req, token).await?;
        self.handler.on_hello(name);
        // Restart recovery (§16.2): the plugin knows nothing about the
        // pairs it had; every row is offered again, and a refusal now is
        // the pair's state, not an error for the plugin.
        if let Some(addr) = self.registry.ready_addr(name) {
            for (agent, row) in self.registry.rows_for_plugin(name) {
                let req = ActivateRequest {
                    agent: agent.to_string(),
                    config: row.config.clone(),
                };
                let activation = match self.client.activate(&addr, &req).await {
                    Ok(()) => PluginActivation::active(),
                    Err(e) => {
                        tracing::warn!(plugin = %name, agent = %agent, "activation rejected at hello: {e}");
                        PluginActivation::rejected(Self::activation_message(&e))
                    }
                };
                self.registry.set_state(&agent, name, activation);
            }
        }
        self.bump();
        Ok(response)
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

    /// `POST` (`replace == false`): 409 unless the fleet is absent or
    /// settled `Down`. `PUT` (`replace == true`): 404 when absent. Both
    /// are answered before any plugin is called, so a refused apply leaves
    /// no plugin holding a config the fleet never took.
    ///
    /// The activation diff's old side is the fleet's `Active` rows only
    /// (R24): a pair whose row is `Pending` or `Rejected` is offered to the
    /// plugin again by the next `apply`, even when its config is unchanged
    /// — a ready plugin gets an `activate` (and a rejection fails the
    /// apply like any other), a plugin that is not ready leaves it pending.
    pub async fn apply(
        &self,
        name: &FleetName,
        spec: FleetSpec,
        credentials: CredentialBundle,
        replace: bool,
    ) -> Result<FleetRecord, DaemonError> {
        Self::reject_reserved(name)?;
        if spec.name != name.as_str() {
            return Err(DaemonError::Invalid(format!(
                "spec.name {:?} does not match the fleet {name}",
                spec.name
            )));
        }
        Fleet::try_from(spec.clone()).map_err(|e| DaemonError::Invalid(e.to_string()))?;
        // Activation runs before the actor sees the spec (§16.2): a
        // rejection is a 400 and nothing lands. Held for the whole method
        // so two applies of the same fleet cannot interleave.
        let lock = self.fleet_lock(name);
        let _guard = lock.lock().await;
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
        // Whether this is a 409 (POST on a live fleet) or a 404 (PUT on an
        // absent one) is decided *before* any plugin is told anything: a
        // rejected apply must not leave a plugin holding a config the
        // fleet never took. The per-fleet lock is held, and it excludes
        // the only other mutators of this entry, so the write lock below
        // sees the same answer.
        {
            let fleets = self.fleets.read().await;
            match fleets.get(name) {
                Some(h) if !replace && !h.status.borrow().is_down() => {
                    return Err(DaemonError::Conflict);
                }
                None if replace => return Err(DaemonError::NotFound),
                _ => {}
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
                None => rows.push((p.clone(), PluginActivation::pending())),
            }
        }
        let handle = {
            let mut fleets = self.fleets.write().await;
            match fleets.get(name) {
                Some(h) => {
                    if !replace && !h.status.borrow().is_down() {
                        return Err(DaemonError::Conflict);
                    }
                    h.clone()
                }
                None => {
                    if replace {
                        return Err(DaemonError::NotFound);
                    }
                    let h = actor::spawn(
                        name.clone(),
                        FleetRecord::new(spec.clone()),
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
        let (reply, rx) = oneshot::channel();
        handle
            .tx
            .send(Msg::Apply {
                spec,
                credentials,
                reply,
            })
            .await
            .map_err(|_| DaemonError::Internal("fleet task is gone".into()))?;
        let record = rx
            .await
            .map_err(|_| DaemonError::Internal("fleet task dropped the request".into()))?;
        // only the pairs the new spec dropped: a changed pair kept its row
        // and was replaced in place above
        for (agent, plugin) in &d.deactivate {
            self.deactivate_pair(agent, plugin).await;
            self.registry.remove_row(agent, plugin);
        }
        for (p, activation) in rows {
            self.registry.set_row(
                &p.agent,
                &p.plugin,
                ActivationRow {
                    config: p.config,
                    activation,
                },
            );
        }
        self.bump();
        Ok(self.overlay(record))
    }

    pub async fn down(
        &self,
        name: &FleetName,
        keep: Keep,
        purge: bool,
    ) -> Result<FleetRecord, DaemonError> {
        Self::reject_reserved(name)?;
        let lock = self.fleet_lock(name);
        let _guard = lock.lock().await;
        let handle = self
            .fleets
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or(DaemonError::NotFound)?;
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

    pub async fn get(&self, name: &FleetName) -> Option<FleetRecord> {
        if is_reserved_fleet(name.as_str()) {
            return Some(self.plugins.record());
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
        out.push(self.plugins.record());
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
                    .plugins()
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
        assert_eq!(w.daemon.plugins().list().await[0].active_agents, 1);
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
            .apply(&name, s, Default::default(), true)
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
            w.daemon
                .registry()
                .row(&agent, &flow)
                .unwrap()
                .activation
                .state,
            ActivationState::Active
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
        let rows = w.daemon.plugins().list().await;
        assert_eq!(rows[0].message, "degraded: HTTP 503");
        assert_eq!(rows[0].phase, AgentPhase::Ready, "never restarted for it");
        hello(&w, &stub.listen).await;
        assert_eq!(
            w.daemon.plugins().list().await[0].message,
            "",
            "hello clears it"
        );
        let _ = &w.dir;
    }
}
