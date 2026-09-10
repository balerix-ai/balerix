//! One task per fleet (Phase 3 spec §3.2, P3-4): the single writer of that
//! fleet's record, secrets and status. Passes run in `spawn_blocking`;
//! snapshots go out on a `watch` channel; the inbox queues while a pass
//! runs, so a Ready arriving mid-pass lands when the pass ends.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use balerix_api::{CredentialBundle, FleetSpec, Timestamp};
use balerix_core::reconcile::{ReconcileContext, agent_ready, reconcile_pass, set_desired};
use balerix_core::{
    AgentId, AgentRunner, Clock, Desired, Fleet, FleetName, FleetRecord, FleetSecrets, FleetStore,
    HookTarget, Keep, Materializer, ReconcilePolicy, ResolvedAgent, WorkspaceReader,
};
use tokio::sync::{RwLock, mpsc, oneshot, watch};

use crate::metrics::Metrics;
use crate::system_pool::SystemPoolState;
use crate::vault::random_hex;

/// The hook event that means "Claude is up and accepting input".
pub const READY_EVENT: &str = "SessionStart";

/// Floor on the actor's next-wake delay. Without it, a clean pass whose
/// `next_restart_at` is already due (or arrives due, e.g. after a clock
/// jump) computes a zero-length sleep and the loop spins `observe()`
/// continuously — a clean pass leaves `next_restart_at` in place since
/// `plan` emits no step for an unchanged `Running` agent.
const MIN_TICK: Duration = Duration::from_secs(1);

pub enum Msg {
    Apply {
        spec: FleetSpec,
        credentials: CredentialBundle,
        reply: oneshot::Sender<FleetRecord>,
    },
    Down {
        keep: Keep,
        purge: bool,
        reply: oneshot::Sender<FleetRecord>,
    },
    /// Hold (or release) one agent in the record's `stopped` set (plugins
    /// spec §16.4). Replied to after the pass, so `restart` (stop, then
    /// resume) is two ordered round trips.
    SetStopped {
        agent: AgentId,
        stopped: bool,
        reply: oneshot::Sender<FleetRecord>,
    },
    Event {
        agent: AgentId,
        name: String,
        at: Timestamp,
    },
}

#[derive(Clone)]
pub struct FleetHandle {
    pub tx: mpsc::Sender<Msg>,
    pub status: watch::Receiver<FleetRecord>,
}

/// Everything every actor shares read-only.
pub struct Ports {
    pub materializer: Arc<dyn Materializer>,
    pub runner: Arc<dyn AgentRunner>,
    pub clock: Arc<dyn Clock>,
    pub store: Arc<dyn FleetStore>,
    /// Read-only worktree access for the plugin host's workspace routes
    /// (Spec C §3.1). Not used by the reconciler.
    pub workspace: Arc<dyn WorkspaceReader>,
    pub policy: ReconcilePolicy,
    /// `http://127.0.0.1:<port>`; every agent's hooks post here.
    pub hook_url: String,
    pub resync: Duration,
}

/// Agent id → the bearer secret its hooks present. Ingress authenticates
/// against this; actors keep it current.
pub type SecretIndex = Arc<RwLock<HashMap<AgentId, String>>>;

#[derive(Clone)]
pub struct Shared {
    pub hook_secrets: SecretIndex,
    pub metrics: Metrics,
    /// An actor announces its own name here after a purge; the registry
    /// drops the handle.
    pub purged: mpsc::Sender<FleetName>,
    /// The daemon pool's live state. Read at the top of every pass; a pass
    /// does not run unless this is `Ready` (Spec F §5).
    pub system_pool: watch::Receiver<SystemPoolState>,
}

/// The shared state, the purge inbox and the readiness sender the
/// `SystemPool` actor is spawned with (`crate::system_pool::spawn`). That
/// sender is the channel's only writer: drop it and every fleet gates shut
/// for good, so it belongs to whoever owns the pool.
pub fn shared(
    metrics: Metrics,
) -> (
    Shared,
    mpsc::Receiver<FleetName>,
    watch::Sender<SystemPoolState>,
) {
    let (purged, rx) = mpsc::channel(16);
    let (pool_tx, pool_rx) = watch::channel(SystemPoolState::Pending);
    (
        Shared {
            hook_secrets: Arc::default(),
            metrics,
            purged,
            system_pool: pool_rx,
        },
        rx,
        pool_tx,
    )
}

/// Resolves when the daemon pool's state changes. A closed channel — the
/// pool's owner is gone — never resolves, rather than resolving instantly
/// for ever and spinning the actor's loop at full tilt.
async fn pool_changed(rx: &mut watch::Receiver<SystemPoolState>) {
    if rx.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Starts the actor. `initial_pass` is true for records loaded at startup
/// (reconcile what tmux still has) and false for a fresh `POST`, whose
/// `Apply` triggers the first pass.
pub fn spawn(
    name: FleetName,
    record: FleetRecord,
    secrets: FleetSecrets,
    ports: Arc<Ports>,
    shared: Shared,
    initial_pass: bool,
) -> FleetHandle {
    let (tx, rx) = mpsc::channel(1024);
    let (publish, status) = watch::channel(record.clone());
    let actor = Actor {
        name,
        record,
        secrets,
        ports,
        shared,
        publish,
        rx,
        last_pass_clean: true,
    };
    tokio::spawn(actor.run(initial_pass));
    FleetHandle { tx, status }
}

struct Actor {
    name: FleetName,
    record: FleetRecord,
    secrets: FleetSecrets,
    ports: Arc<Ports>,
    shared: Shared,
    publish: watch::Sender<FleetRecord>,
    rx: mpsc::Receiver<Msg>,
    /// A pass with a failed step retries at the resync cadence, not at
    /// `next_restart_at`: a failing clone must not spin.
    last_pass_clean: bool,
}

impl Actor {
    async fn run(mut self, initial_pass: bool) {
        // Whatever the pool's state is now is where this actor starts from;
        // only a change *after* that is a reason to wake. Without marking it
        // seen, an actor spawned once the pool is already ready reads its
        // clone's stale version as an unseen change and runs a pass nobody
        // asked for — `initial_pass` is false for a fresh `POST` precisely
        // so that the `Apply` is the first pass.
        self.shared.system_pool.borrow_and_update();
        self.seed_index().await;
        if initial_pass && !self.record.is_down() {
            self.pass().await;
        }
        loop {
            let deadline = self.deadline();
            let msg = tokio::select! {
                m = self.rx.recv() => match m {
                    Some(m) => Some(m),
                    None => return,
                },
                () = tokio::time::sleep_until(deadline) => None,
                // A pool that goes ready two seconds in must not leave every
                // fleet idle for a whole resync interval.
                () = pool_changed(&mut self.shared.system_pool) => None,
            };
            match msg {
                Some(Msg::Apply {
                    spec,
                    credentials,
                    reply,
                }) => {
                    self.apply(spec, credentials).await;
                    let _ = reply.send(self.record.clone());
                    self.pass().await;
                }
                Some(Msg::Down { keep, purge, reply }) => {
                    self.record.desired = Desired::Down { keep, purge };
                    self.persist().await;
                    self.publish();
                    let _ = reply.send(self.record.clone());
                    self.pass().await;
                }
                Some(Msg::SetStopped {
                    agent,
                    stopped,
                    reply,
                }) => {
                    let key = agent.to_string();
                    if stopped {
                        self.record.stopped.insert(key);
                    } else {
                        self.record.stopped.remove(&key);
                    }
                    self.persist().await;
                    self.publish();
                    self.pass().await;
                    let _ = reply.send(self.record.clone());
                }
                Some(Msg::Event { agent, name, at }) => self.event(agent, name, at).await,
                None => self.pass().await,
            }
            if matches!(self.record.desired, Desired::Down { purge: true, .. })
                && self.record.is_down()
            {
                self.purge().await;
                return;
            }
        }
    }

    fn deadline(&self) -> tokio::time::Instant {
        let now = tokio::time::Instant::now();
        let resync = now + self.ports.resync;
        let floor = now + MIN_TICK;
        if !self.last_pass_clean {
            return resync;
        }
        let now_ts = self.ports.clock.now();
        match self
            .record
            .status
            .agents
            .values()
            .filter_map(|a| a.next_restart_at)
            .min()
        {
            Some(due) => (now + Duration::from_secs(due.0.saturating_sub(now_ts.0)))
                .max(floor)
                .min(resync),
            None => resync,
        }
    }

    /// Ids of the agents the current spec wants, or none if the spec does
    /// not convert (the API validated it; a stored record may not).
    fn wanted_agents(&self) -> Vec<AgentId> {
        Fleet::try_from(self.record.spec.clone())
            .map(|f| {
                ResolvedAgent::from_fleet(&f)
                    .into_iter()
                    .map(|a| a.id)
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn seed_index(&self) {
        let mut idx = self.shared.hook_secrets.write().await;
        for (id, secret) in &self.secrets.hook_secrets {
            if let Ok(id) = id.parse::<AgentId>() {
                idx.insert(id, secret.clone());
            }
        }
    }

    async fn apply(&mut self, spec: FleetSpec, credentials: CredentialBundle) {
        self.record.generation += 1;
        self.record.spec = spec;
        self.record.desired = Desired::Up;
        set_desired(&mut self.record.status, self.record.generation);
        self.secrets.credentials = credentials;
        let wanted = self.wanted_agents();
        // an Apply always wins over a plugin's stop (plugins spec §16.4)
        self.record
            .stopped
            .retain(|k| !wanted.iter().any(|id| id.to_string() == *k));
        let mut next = BTreeMap::new();
        for id in &wanted {
            let key = id.to_string();
            let secret = self
                .secrets
                .hook_secrets
                .get(&key)
                .cloned()
                .unwrap_or_else(|| random_hex(32));
            next.insert(key, secret);
        }
        self.secrets.hook_secrets = next;
        {
            let mut idx = self.shared.hook_secrets.write().await;
            idx.retain(|id, _| id.fleet != self.name);
            for id in &wanted {
                if let Some(s) = self.secrets.hook_secrets.get(&id.to_string()) {
                    idx.insert(id.clone(), s.clone());
                }
            }
        }
        self.persist().await;
        self.publish();
    }

    async fn event(&mut self, agent: AgentId, name: String, at: Timestamp) {
        if name == READY_EVENT {
            // The one line that dates an agent's readiness: the tick that
            // follows logs the phase, not when it was reached.
            tracing::info!(agent = %agent, fleet = %self.name, "agent ready ({READY_EVENT} received)");
            agent_ready(&mut self.record.status, &agent, at);
            self.persist().await;
        } else if let Some(a) = self.record.status.agents.get_mut(&agent.to_string()) {
            a.last_event_at = Some(at);
        }
        self.publish();
    }

    async fn pass(&mut self) {
        // Spec F §5: the daemon pool is a precondition, not the fleet's
        // fault. Skip the pass and leave status alone; a crew marked failed
        // here would move restart counters for a daemon-level condition.
        if *self.shared.system_pool.borrow() != SystemPoolState::Ready {
            tracing::debug!(fleet = %self.name, "skipping the pass: daemon mise pool not ready");
            return;
        }
        let ports = self.ports.clone();
        let name = self.name.clone();
        let desired = match self.record.desired {
            Desired::Up => Some(Fleet::try_from(self.record.spec.clone())),
            Desired::Down { .. } => None,
        };
        let keep = match self.record.desired {
            Desired::Down { keep, .. } => keep,
            Desired::Up => Keep::default(),
        };
        let before = self.record.status.clone();
        let mut status = before.clone();
        let creds = self.secrets.credentials.clone();
        let hook_secrets = self.secrets.hook_secrets.clone();
        let stopped: BTreeSet<AgentId> = self
            .record
            .stopped
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();
        let started = Instant::now();
        let joined = tokio::task::spawn_blocking(move || {
            let fleet = match desired {
                Some(Ok(f)) => Some(f),
                Some(Err(e)) => return (status, Err(format!("{name}: invalid stored spec: {e}"))),
                None => None,
            };
            let hooks = |id: &AgentId| HookTarget {
                url: ports.hook_url.clone(),
                secret: hook_secrets
                    .get(&id.to_string())
                    .cloned()
                    .unwrap_or_default(),
            };
            let ctx = ReconcileContext {
                fleet: &name,
                desired: fleet.as_ref(),
                keep,
                stopped: &stopped,
                materializer: ports.materializer.as_ref(),
                runner: ports.runner.as_ref(),
                creds: &creds,
                hooks: &hooks,
                policy: &ports.policy,
                clock: ports.clock.as_ref(),
            };
            let outcome = reconcile_pass(&mut status, &ctx).map_err(|e| e.to_string());
            (status, outcome)
        })
        .await;
        let secs = started.elapsed().as_secs_f64();
        let clean = match joined {
            Ok((status, Ok((plan, report)))) => {
                self.record.status = status;
                for (step, err) in &report.failures {
                    tracing::warn!(fleet = %self.name, step = %step, "step failed: {err}");
                }
                tracing::info!(
                    fleet = %self.name,
                    steps = plan.len(),
                    failed = report.failures.len(),
                    skipped = report.skipped.len(),
                    phase = ?self.record.status.phase,
                    "reconciled"
                );
                report.all_ok()
            }
            Ok((status, Err(e))) => {
                self.record.status = status;
                tracing::error!(fleet = %self.name, "pass failed: {e}");
                false
            }
            Err(e) => {
                tracing::error!(fleet = %self.name, "reconcile task panicked: {e}");
                false
            }
        };
        self.last_pass_clean = clean;
        self.shared
            .metrics
            .reconcile(self.name.as_str(), secs, clean);
        for (id, a) in &self.record.status.agents {
            let prev = before.agents.get(id).map_or(0, |p| p.restarts);
            if a.restarts > prev
                && let Ok(aid) = id.parse::<AgentId>()
            {
                self.shared.metrics.restart(&aid);
            }
        }
        self.persist().await;
        self.publish();
    }

    async fn persist(&self) {
        let store = self.ports.store.clone();
        let record = self.record.clone();
        let secrets = self.secrets.clone();
        match tokio::task::spawn_blocking(move || store.put(&record, &secrets)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(fleet = %self.name, "persist failed: {e}"),
            Err(e) => tracing::error!(fleet = %self.name, "persist task panicked: {e}"),
        }
    }

    fn publish(&self) {
        let _ = self.publish.send(self.record.clone());
    }

    async fn purge(&mut self) {
        let store = self.ports.store.clone();
        let name = self.name.clone();
        match tokio::task::spawn_blocking(move || store.purge(&name)).await {
            Ok(Ok(())) => tracing::info!(fleet = %self.name, "purged"),
            Ok(Err(e)) => tracing::error!(fleet = %self.name, "purge failed: {e}"),
            Err(e) => tracing::error!(fleet = %self.name, "purge task panicked: {e}"),
        }
        self.shared
            .hook_secrets
            .write()
            .await
            .retain(|id, _| id.fleet != self.name);
        let _ = self.shared.purged.send(self.name.clone()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Harness;
    use balerix_api::{AgentPhase, AgentSettings, AgentStatus, CrewSpec, FleetPhase, GitSettings};
    use balerix_core::ProcessState;
    use std::collections::{BTreeMap, BTreeSet};

    fn spec(agents: &[&str]) -> FleetSpec {
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
                        .map(|a| (a.to_string(), AgentSettings::default()))
                        .collect(),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        }
    }

    fn id(s: &str) -> AgentId {
        s.parse().unwrap()
    }

    async fn wait(
        rx: &mut watch::Receiver<FleetRecord>,
        pred: impl Fn(&FleetRecord) -> bool,
    ) -> FleetRecord {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if pred(&rx.borrow()) {
                    return rx.borrow().clone();
                }
                rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap_or_else(|_| panic!("condition not reached; last: {:#?}", rx.borrow().status))
    }

    async fn apply(h: &FleetHandle, spec: FleetSpec) -> FleetRecord {
        let (tx, rx) = oneshot::channel();
        h.tx.send(Msg::Apply {
            spec,
            credentials: CredentialBundle::default(),
            reply: tx,
        })
        .await
        .unwrap();
        rx.await.unwrap()
    }

    async fn down(h: &FleetHandle, purge: bool) -> FleetRecord {
        let (tx, rx) = oneshot::channel();
        h.tx.send(Msg::Down {
            keep: Keep::default(),
            purge,
            reply: tx,
        })
        .await
        .unwrap();
        rx.await.unwrap()
    }

    async fn set_stopped(h: &FleetHandle, agent: &str, stopped: bool) -> FleetRecord {
        let (tx, rx) = oneshot::channel();
        h.tx.send(Msg::SetStopped {
            agent: id(agent),
            stopped,
            reply: tx,
        })
        .await
        .unwrap();
        rx.await.unwrap()
    }

    /// Everything a test must hold for as long as the actor runs. The
    /// `watch::Sender` is one of them: dropping it closes the readiness
    /// channel the actor selects on.
    type Started = (
        FleetHandle,
        Shared,
        mpsc::Receiver<FleetName>,
        watch::Sender<SystemPoolState>,
    );

    /// An actor over a pool that is already `Ready` — what every test but
    /// the gate's own wants.
    fn start(h: &Harness) -> Started {
        start_with_pool(h, SystemPoolState::Ready)
    }

    fn start_with_pool(h: &Harness, state: SystemPoolState) -> Started {
        let (shared, purged, pool) = shared(Metrics::new().unwrap());
        pool.send(state).unwrap();
        let handle = spawn(
            "f".parse().unwrap(),
            FleetRecord::new(spec(&[])),
            FleetSecrets::default(),
            h.ports.clone(),
            shared.clone(),
            false,
        );
        (handle, shared, purged, pool)
    }

    /// A `Shared` whose pool is already `Ready`, for the tests that spawn
    /// their actor by hand. The sender comes back so the caller can keep
    /// the channel open.
    fn ready_shared() -> (
        Shared,
        mpsc::Receiver<FleetName>,
        watch::Sender<SystemPoolState>,
    ) {
        let (shared, purged, pool) = shared(Metrics::new().unwrap());
        pool.send(SystemPoolState::Ready).unwrap();
        (shared, purged, pool)
    }

    /// A round trip that lands *after* the pass the previous message
    /// triggered: the actor handles one message at a time and replies to
    /// this one only once that pass has finished. A skipped pass publishes
    /// nothing, so this is how these tests know one has happened.
    async fn after_pass(h: &FleetHandle) {
        set_stopped(h, "f/c/a", false).await;
    }

    #[tokio::test]
    async fn apply_reconciles_mints_secrets_and_persists() {
        let h = Harness::new(Duration::from_secs(3600));
        let (handle, shared, _purged, _pool) = start(&h);
        let reply = apply(&handle, spec(&["a", "b"])).await;
        assert_eq!(reply.generation, 1);
        assert_eq!(reply.desired, Desired::Up);
        // persist happens before the reply (spec §3.2): the store already
        // has generation 1 and both freshly minted secrets, before the pass
        // that follows the reply has had a chance to run.
        let (stored_before_pass, secrets_before_pass) = h.store.get("f").unwrap();
        assert_eq!(stored_before_pass.generation, 1);
        assert_eq!(secrets_before_pass.hook_secrets.len(), 2);
        let mut rx = handle.status.clone();
        let rec = wait(&mut rx, |r| r.status.observed_generation == 1).await;
        assert_eq!(rec.status.agents["f/c/a"].phase, AgentPhase::Starting);
        assert_eq!(rec.status.phase, FleetPhase::Reconciling);
        assert!(h.runner.calls().contains(&"ensure_agent f/c/b".to_string()));
        let idx = shared.hook_secrets.read().await;
        assert_eq!(idx.len(), 2);
        assert_eq!(idx[&id("f/c/a")].len(), 64, "32 random bytes as hex");
        let (stored, secrets) = h.store.get("f").unwrap();
        assert_eq!(stored.generation, 1);
        assert_eq!(secrets.hook_secrets.len(), 2);
        assert_eq!(secrets.hook_secrets["f/c/a"], idx[&id("f/c/a")]);
    }

    #[tokio::test]
    async fn session_start_readies_an_agent_and_a_new_apply_keeps_its_secret() {
        let h = Harness::new(Duration::from_secs(3600));
        let (handle, shared, _purged, _pool) = start(&h);
        apply(&handle, spec(&["a", "b"])).await;
        let mut rx = handle.status.clone();
        wait(&mut rx, |r| r.status.observed_generation == 1).await;

        handle
            .tx
            .send(Msg::Event {
                agent: id("f/c/a"),
                name: READY_EVENT.into(),
                at: Timestamp(5),
            })
            .await
            .unwrap();
        let rec = wait(&mut rx, |r| {
            r.status.agents["f/c/a"].phase == AgentPhase::Ready
        })
        .await;
        assert_eq!(rec.status.agents["f/c/a"].last_event_at, Some(Timestamp(5)));
        assert_eq!(
            h.store.get("f").unwrap().0.status.agents["f/c/a"].phase,
            AgentPhase::Ready,
            "a phase change is persisted"
        );

        handle
            .tx
            .send(Msg::Event {
                agent: id("f/c/b"),
                name: "PreToolUse".into(),
                at: Timestamp(9),
            })
            .await
            .unwrap();
        let rec = wait(&mut rx, |r| {
            r.status.agents["f/c/b"].last_event_at == Some(Timestamp(9))
        })
        .await;
        assert_eq!(
            rec.status.agents["f/c/b"].phase,
            AgentPhase::Starting,
            "only SessionStart readies"
        );

        let before = shared.hook_secrets.read().await[&id("f/c/a")].clone();
        let reply = apply(&handle, spec(&["a"])).await;
        assert_eq!(reply.generation, 2);
        wait(&mut rx, |r| r.status.observed_generation == 2).await;
        let idx = shared.hook_secrets.read().await;
        assert_eq!(idx.len(), 1);
        assert_eq!(
            idx[&id("f/c/a")],
            before,
            "a surviving agent keeps its secret"
        );
        assert!(h.runner.calls().contains(&"stop_agent f/c/b".to_string()));
        assert_eq!(h.store.get("f").unwrap().1.hook_secrets.len(), 1);
    }

    #[tokio::test]
    async fn down_settles_then_purge_removes_everything_and_ends_the_task() {
        let h = Harness::new(Duration::from_secs(3600));
        let (handle, shared, mut purged, _pool) = start(&h);
        apply(&handle, spec(&["a"])).await;
        let mut rx = handle.status.clone();
        wait(&mut rx, |r| r.status.observed_generation == 1).await;

        let reply = down(&handle, false).await;
        assert!(matches!(reply.desired, Desired::Down { purge: false, .. }));
        // persist happens before the reply (spec §3.2): the store already
        // has the new `desired` before the pass that follows the reply has
        // had a chance to settle the fleet to `Down`.
        assert!(matches!(
            h.store.get("f").unwrap().0.desired,
            Desired::Down { purge: false, .. }
        ));
        let rec = wait(&mut rx, |r| r.status.phase == FleetPhase::Down).await;
        assert!(rec.is_down());
        assert!(h.runner.observed().crews.is_empty());
        assert_eq!(h.store.names(), vec!["f"], "record kept until purge");

        down(&handle, true).await;
        let name = tokio::time::timeout(Duration::from_secs(5), purged.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(name.as_str(), "f");
        assert!(h.store.names().is_empty());
        assert!(shared.hook_secrets.read().await.is_empty());
        tokio::time::timeout(Duration::from_secs(5), handle.tx.closed())
            .await
            .expect("actor task ends after purge");
    }

    #[tokio::test]
    async fn an_exit_is_noted_then_restarted_on_the_timer() {
        let policy = ReconcilePolicy {
            max_restarts: 5,
            backoff_base_secs: 0,
            backoff_cap_secs: 0,
        };
        let h = Harness::with_policy(Duration::from_millis(50), policy);
        let (handle, shared, _purged, _pool) = start(&h);
        apply(&handle, spec(&["a"])).await;
        let mut rx = handle.status.clone();
        wait(&mut rx, |r| r.status.observed_generation == 1).await;

        h.runner
            .set_state(&id("f/c/a"), ProcessState::Exited { code: Some(1) });
        let rec = wait(&mut rx, |r| r.status.agents["f/c/a"].restarts == 1).await;
        assert!(
            rec.status.agents["f/c/a"]
                .message
                .starts_with("exited with status 1")
        );
        wait(&mut rx, |r| {
            r.status.agents["f/c/a"].next_restart_at.is_none()
                && h.runner
                    .calls()
                    .iter()
                    .filter(|c| *c == "ensure_agent f/c/a")
                    .count()
                    == 2
        })
        .await;
        assert!(
            shared
                .metrics
                .encode()
                .contains("balerix_agent_restarts_total{agent=\"a\",crew=\"c\",fleet=\"f\"} 1")
        );
    }

    #[tokio::test]
    async fn a_loaded_record_seeds_the_secret_index_and_reconciles_once() {
        let h = Harness::new(Duration::from_secs(3600));
        let (shared, _purged, _pool) = ready_shared();
        let mut record = FleetRecord::new(spec(&["a"]));
        record.generation = 3;
        record.status.generation = 3;
        let secrets = FleetSecrets {
            hook_secrets: BTreeMap::from([("f/c/a".to_string(), "kept".to_string())]),
            ..FleetSecrets::default()
        };
        let handle = spawn(
            "f".parse().unwrap(),
            record,
            secrets,
            h.ports.clone(),
            shared.clone(),
            true,
        );
        let mut rx = handle.status.clone();
        let rec = wait(&mut rx, |r| r.status.observed_generation == 3).await;
        assert_eq!(rec.status.agents["f/c/a"].phase, AgentPhase::Starting);
        assert_eq!(shared.hook_secrets.read().await[&id("f/c/a")], "kept");
    }

    /// A stale `next_restart_at` left over on an agent the runner reports as
    /// still `Running` (e.g. a record loaded after a crash, before this
    /// agent's own `Start` step ever clears the field) must not make
    /// `deadline()` spin `observe()` with no delay: `MIN_TICK` floors the
    /// wake even though the naive computation is due "now".
    #[tokio::test]
    async fn a_stale_past_next_restart_at_on_a_running_agent_does_not_spin_observe() {
        let h = Harness::new(Duration::from_secs(10));
        let s = spec(&["a"]);
        let fleet = Fleet::try_from(s.clone()).unwrap();
        let hash = ResolvedAgent::from_fleet(&fleet)[0].hash();
        h.runner
            .set_state(&id("f/c/a"), ProcessState::Running { pid: 1 });
        let mut record = FleetRecord::new(s);
        record.generation = 1;
        record.status.generation = 1;
        record.status.agents.insert(
            "f/c/a".to_string(),
            AgentStatus {
                applied_hash: Some(hash),
                next_restart_at: Some(Timestamp(500)), // already in FakeClock's past (1_000)
                ..AgentStatus::default()
            },
        );
        let (shared, _purged, _pool) = ready_shared();
        let handle = spawn(
            "f".parse().unwrap(),
            record,
            FleetSecrets::default(),
            h.ports.clone(),
            shared,
            true,
        );
        let mut rx = handle.status.clone();
        wait(&mut rx, |r| r.status.observed_generation == 1).await;

        let observe_calls = || {
            h.runner
                .calls()
                .iter()
                .filter(|c| *c == "observe f")
                .count()
        };
        let before = observe_calls();
        assert!(before >= 1, "the initial pass must have observed");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let after = observe_calls();
        assert!(
            after - before <= 1,
            "expected the wake to be floored at MIN_TICK; observe grew by {} within 300ms",
            after - before
        );
    }

    /// Spec §3.2: a failed `observe()` is logged, counted, leaves status
    /// unchanged, and the next tick retries at the resync cadence — even
    /// when an agent already has a due `next_restart_at`, a failing pass
    /// must not spin faster than resync.
    #[tokio::test]
    async fn a_failed_observe_leaves_status_unchanged_counts_it_and_retries_at_resync() {
        let policy = ReconcilePolicy {
            max_restarts: 5,
            backoff_base_secs: 0,
            backoff_cap_secs: 0,
        };
        let h = Harness::with_policy(Duration::from_millis(200), policy);
        let (handle, shared, _purged, _pool) = start(&h);
        apply(&handle, spec(&["a"])).await;
        let mut rx = handle.status.clone();
        wait(&mut rx, |r| r.status.observed_generation == 1).await;

        h.runner
            .set_state(&id("f/c/a"), ProcessState::Exited { code: Some(1) });
        let before = wait(&mut rx, |r| {
            r.status.agents["f/c/a"].next_restart_at.is_some()
        })
        .await;

        let observe_calls = || {
            h.runner
                .calls()
                .iter()
                .filter(|c| *c == "observe f")
                .count()
        };
        h.runner.set_fail_observe(true);
        let base = observe_calls();

        // (c) the due restart does not make the failing pass fire early.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            observe_calls(),
            base,
            "a failing pass must not fire before the resync cadence"
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if observe_calls() > base {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the failing pass never ran");
        // let the failing pass's persist()/publish() land.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // (a) a failed observe leaves the published status unchanged.
        assert_eq!(
            rx.borrow().status,
            before.status,
            "status must be unchanged after a failed observe"
        );

        // (b) it is counted.
        let encoded = shared.metrics.encode();
        assert!(
            encoded.contains("balerix_reconcile_errors_total{fleet=\"f\"} 1"),
            "{encoded}"
        );
    }

    #[tokio::test]
    async fn set_stopped_stops_resumes_and_apply_clears_it() {
        let h = Harness::new(Duration::from_secs(3600));
        let (handle, _shared, _purged, _pool) = start(&h);
        apply(&handle, spec(&["a", "b"])).await;
        let mut rx = handle.status.clone();
        wait(&mut rx, |r| r.status.observed_generation == 1).await;

        let rec = set_stopped(&handle, "f/c/a", true).await;
        assert_eq!(rec.stopped, BTreeSet::from(["f/c/a".to_string()]));
        assert_eq!(rec.status.agents["f/c/a"].phase, AgentPhase::Stopped);
        assert_eq!(rec.status.agents["f/c/b"].phase, AgentPhase::Starting);
        assert!(h.runner.calls().contains(&"stop_agent f/c/a".to_string()));
        assert_eq!(h.store.get("f").unwrap().0.stopped.len(), 1, "persisted");
        let ensure_a = || {
            h.runner
                .calls()
                .iter()
                .filter(|c| *c == "ensure_agent f/c/a")
                .count()
        };
        assert_eq!(ensure_a(), 1);

        let rec = set_stopped(&handle, "f/c/a", false).await;
        assert!(rec.stopped.is_empty());
        assert_eq!(rec.status.agents["f/c/a"].phase, AgentPhase::Starting);
        assert_eq!(rec.status.agents["f/c/a"].restarts, 0);
        assert_eq!(ensure_a(), 2, "resumed in the same message's pass");

        set_stopped(&handle, "f/c/b", true).await;
        let rec = apply(&handle, spec(&["a", "b"])).await;
        assert!(
            rec.stopped.is_empty(),
            "an Apply clears every declared agent"
        );
        wait(&mut rx, |r| r.status.observed_generation == 2).await;
        assert!(
            h.runner
                .calls()
                .iter()
                .filter(|c| *c == "ensure_agent f/c/b")
                .count()
                >= 2
        );
    }

    /// A fleet created by a fresh `POST` gets its first pass from its
    /// `Apply` and never before it: that is what `initial_pass = false`
    /// buys, and callers rely on it — `Daemon::apply` spawns the actor with
    /// a record the `Apply` is about to replace.
    ///
    /// The readiness channel is the trap. `Shared` is cloned into every
    /// actor and a `watch::Receiver` clone keeps the *original's* seen
    /// version, which nothing in the daemon ever advances. An actor spawned
    /// once the pool is already `Ready` — the normal case after startup,
    /// and what `start_with_pool` reproduces by sending before it spawns —
    /// therefore reads that as an unseen change, wakes on it and runs a
    /// pass nobody asked for.
    ///
    /// Nothing is sent to this actor, and its resync is an hour: a
    /// readiness wake is the only branch of its select that can be ready,
    /// so the window below cannot fail spuriously. It can only be slow.
    #[tokio::test]
    async fn a_new_fleet_runs_no_pass_before_its_first_apply() {
        let h = Harness::new(Duration::from_secs(3600));
        let (handle, _shared, _purged, _pool) = start_with_pool(&h, SystemPoolState::Ready);
        let mut rx = handle.status.clone();

        assert!(
            tokio::time::timeout(Duration::from_secs(1), rx.changed())
                .await
                .is_err(),
            "the actor published a snapshot before it was asked to do anything"
        );
        assert!(
            h.runner.calls().is_empty(),
            "no pass may have run yet — every pass observes: {:?}",
            h.runner.calls()
        );
        assert!(
            h.materializer.calls().is_empty(),
            "nor may anything have been materialized: {:?}",
            h.materializer.calls()
        );

        // And the `Apply` is the first pass, over the spec it carries.
        apply(&handle, spec(&["a"])).await;
        wait(&mut rx, |r| r.status.observed_generation == 1).await;
        assert!(h.runner.calls().contains(&"ensure_agent f/c/a".to_string()));
    }

    /// Spec F §5: the daemon pool is a precondition the fleet cannot
    /// influence, so an unready pool skips the pass rather than failing it.
    #[tokio::test]
    async fn a_pass_is_skipped_while_the_system_pool_is_not_ready() {
        let h = Harness::new(Duration::from_secs(3600));
        let (handle, shared, _purged, _pool) = start_with_pool(&h, SystemPoolState::Pending);
        let applied = apply(&handle, spec(&["a"])).await;
        after_pass(&handle).await;

        assert!(
            h.materializer.calls().is_empty(),
            "an unready pool must stop the pass before it materializes anything: {:?}",
            h.materializer.calls()
        );
        assert!(
            h.runner.calls().is_empty(),
            "nor may it reach the runner: {:?}",
            h.runner.calls()
        );
        // A skipped pass is not a failed pass: no crew failed, no counter
        // moved. Reusing the failure path here would move restart counters
        // for a daemon-level condition.
        assert_eq!(
            handle.status.borrow().status,
            applied.status,
            "a skipped pass leaves status exactly as the apply left it"
        );
        let encoded = shared.metrics.encode();
        assert!(
            !encoded.contains("balerix_reconcile_errors_total{fleet=\"f\"}"),
            "a skip is not an error: {encoded}"
        );
        assert!(
            !encoded.contains("balerix_reconcile_duration_seconds_count{fleet=\"f\"}"),
            "a skipped pass is not observed as a pass: {encoded}"
        );
    }

    #[tokio::test]
    async fn a_pass_runs_once_the_system_pool_is_ready() {
        let h = Harness::new(Duration::from_secs(3600));
        let (handle, _shared, _purged, _pool) = start_with_pool(&h, SystemPoolState::Ready);
        apply(&handle, spec(&["a"])).await;
        let mut rx = handle.status.clone();
        wait(&mut rx, |r| r.status.observed_generation == 1).await;
        assert!(
            !h.materializer.calls().is_empty(),
            "a ready pool lets the pass run"
        );
    }

    #[tokio::test]
    async fn readiness_is_re_read_every_pass_not_latched() {
        let h = Harness::new(Duration::from_secs(3600));
        let (handle, _shared, _purged, pool) = start_with_pool(&h, SystemPoolState::Ready);
        apply(&handle, spec(&["a"])).await;
        let mut rx = handle.status.clone();
        wait(&mut rx, |r| r.status.observed_generation == 1).await;
        let after_first = h.materializer.calls().len();
        assert!(after_first > 0);

        pool.send(SystemPoolState::Unready {
            reason: "gone".to_string(),
        })
        .unwrap();
        apply(&handle, spec(&["a", "b"])).await;
        after_pass(&handle).await;
        assert_eq!(
            h.materializer.calls().len(),
            after_first,
            "readiness is live: a pool that goes away must stop later passes too (F-4)"
        );
    }

    /// The gate's second rule: a skipped pass waits on whichever comes
    /// first, readiness or the tick. The resync here is an hour, so only
    /// the readiness change can make this pass run.
    #[tokio::test]
    async fn a_pool_that_becomes_ready_wakes_a_waiting_fleet_before_the_next_tick() {
        let h = Harness::new(Duration::from_secs(3600));
        let (handle, _shared, _purged, pool) = start_with_pool(&h, SystemPoolState::Pending);
        apply(&handle, spec(&["a"])).await;
        after_pass(&handle).await;
        assert!(
            h.materializer.calls().is_empty(),
            "the pool is not ready yet"
        );

        pool.send(SystemPoolState::Ready).unwrap();
        let mut rx = handle.status.clone();
        wait(&mut rx, |r| r.status.observed_generation == 1).await;
        assert!(
            !h.materializer.calls().is_empty(),
            "readiness alone must wake the fleet"
        );
    }
}
