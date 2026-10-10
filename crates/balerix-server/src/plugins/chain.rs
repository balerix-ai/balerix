//! The interceptor chain and observer delivery (plugins spec §4.3). One
//! `EventHandler` for the daemon: interceptors run in load-list order under
//! a shared budget and fail open; observers get batches from a bounded
//! per-plugin queue and never touch the response.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use balerix_api::{
    CHAIN_BUDGET_MS, EventBatch, HookEvent, InterceptRequest, OBSERVER_BATCH, OBSERVER_QUEUE,
};
use balerix_core::{AgentId, AgentName, EventHandler, HandlerFuture, Outcome};
use tokio::sync::Notify;

use super::client::PluginClient;
use super::registry::PluginRegistry;
use crate::metrics::{DropReason, Metrics};

/// How long a delivery task waits for a batch to fill before sending.
pub const BATCH_WINDOW: Duration = Duration::from_millis(100);

pub struct ObserverQueue {
    plugin: AgentName,
    buf: Mutex<VecDeque<HookEvent>>,
    notify: Notify,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ObserverQueue {
    pub fn new(plugin: AgentName) -> Arc<Self> {
        Arc::new(Self {
            plugin,
            buf: Mutex::new(VecDeque::with_capacity(OBSERVER_QUEUE)),
            notify: Notify::new(),
        })
    }

    /// Appends; on overflow the oldest event goes and `true` comes back.
    pub fn push(&self, event: HookEvent) -> bool {
        let dropped = {
            let mut b = lock(&self.buf);
            let dropped = if b.len() >= OBSERVER_QUEUE {
                b.pop_front();
                true
            } else {
                false
            };
            b.push_back(event);
            dropped
        };
        self.notify.notify_one();
        dropped
    }

    pub fn clear(&self) {
        lock(&self.buf).clear();
    }

    pub fn len(&self) -> usize {
        lock(&self.buf).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn drain(&self, max: usize) -> Vec<HookEvent> {
        let mut b = lock(&self.buf);
        let n = max.min(b.len());
        b.drain(..n).collect()
    }

    /// The delivery loop: wait for something, give the batch `BATCH_WINDOW`
    /// to fill (or `OBSERVER_BATCH`), post it. A plugin that stopped being
    /// ready loses the batch (§4.3: no catch-up).
    async fn deliver(
        self: Arc<Self>,
        registry: Arc<PluginRegistry>,
        client: PluginClient,
        metrics: Metrics,
    ) {
        loop {
            if self.is_empty() {
                self.notify.notified().await;
            }
            let deadline = tokio::time::Instant::now() + BATCH_WINDOW;
            while self.len() < OBSERVER_BATCH {
                tokio::select! {
                    () = self.notify.notified() => {}
                    () = tokio::time::sleep_until(deadline) => break,
                }
            }
            let batch = self.drain(OBSERVER_BATCH);
            if batch.is_empty() {
                continue;
            }
            let Some(addr) = registry.ready_addr(&self.plugin) else {
                metrics.events_dropped(
                    self.plugin.as_str(),
                    DropReason::NotReady,
                    batch.len() as u64,
                );
                continue;
            };
            let n = batch.len();
            if let Err(e) = client.events(&addr, &EventBatch { events: batch }).await {
                tracing::warn!(plugin = %self.plugin, events = n, "observer batch not acknowledged: {e}");
                metrics.events_dropped(self.plugin.as_str(), DropReason::Unacknowledged, n as u64);
            }
        }
    }
}

/// One observing plugin: its queue and the task that delivers it.
struct Observer {
    queue: Arc<ObserverQueue>,
    task: tokio::task::AbortHandle,
}

pub struct PluginEventHandler {
    registry: Arc<PluginRegistry>,
    client: PluginClient,
    metrics: Metrics,
    observers: Mutex<BTreeMap<AgentName, Observer>>,
}

impl PluginEventHandler {
    pub fn new(registry: Arc<PluginRegistry>, client: PluginClient, metrics: Metrics) -> Arc<Self> {
        Arc::new(Self {
            registry,
            client,
            metrics,
            observers: Mutex::new(BTreeMap::new()),
        })
    }

    /// The plugin's queue, its delivery task spawned on first use. Needs
    /// a tokio runtime, which every caller (a request handler) has.
    ///
    /// `None` for a plugin the registry no longer lists: `run` reads the
    /// observers before it gets here, and a sync can remove the plugin in
    /// between. The registry drops a plugin before `remove` is called, and
    /// both this check and `remove` hold the map's lock, so a queue made
    /// here is either seen by `remove` or never made (#13).
    fn queue_for(&self, name: &AgentName) -> Option<Arc<ObserverQueue>> {
        let mut map = lock(&self.observers);
        if let Some(o) = map.get(name) {
            return Some(o.queue.clone());
        }
        if !self.registry.is_installed(name.as_str()) {
            return None;
        }
        let queue = ObserverQueue::new(name.clone());
        let task = tokio::spawn(queue.clone().deliver(
            self.registry.clone(),
            self.client.clone(),
            self.metrics.clone(),
        ))
        .abort_handle();
        map.insert(
            name.clone(),
            Observer {
                queue: queue.clone(),
                task,
            },
        );
        Some(queue)
    }

    /// A plugin that just said hello starts from an empty queue (§4.3).
    pub fn on_hello(&self, name: &AgentName) {
        if let Some(o) = lock(&self.observers).get(name) {
            o.queue.clear();
        }
    }

    /// The plugin is no longer installed: its delivery task ends and its
    /// queue goes, with whatever it still held (#13). A plugin of that
    /// name installed later starts afresh on its first event.
    pub fn remove(&self, name: &AgentName) {
        if let Some(o) = lock(&self.observers).remove(name) {
            o.task.abort();
        }
    }

    pub fn queue_len(&self, name: &AgentName) -> usize {
        lock(&self.observers).get(name).map_or(0, |o| o.queue.len())
    }

    async fn run(&self, event: &HookEvent) -> Outcome {
        let Ok(agent) = event.agent.parse::<AgentId>() else {
            return Outcome::allow();
        };
        let mut outcome = Outcome::allow();
        let deadline = Instant::now() + Duration::from_millis(CHAIN_BUDGET_MS);
        for (name, addr) in self.registry.interceptors(&agent, &event.name) {
            self.metrics
                .plugin_event(name.as_str(), &event.name, "intercept");
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.metrics
                    .intercept(name.as_str(), &event.name, 0.0, Some("timeout"));
                continue;
            }
            let req = InterceptRequest {
                event: event.clone(),
                response_so_far: outcome.response.clone(),
                deadline_ms: u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
            };
            let started = Instant::now();
            match self.client.intercept(&addr, &req, remaining).await {
                Ok(verdict) => {
                    outcome.response = verdict.response;
                    // A merged chain no longer says who asked for what, so
                    // the actions are attributed here (§16.4).
                    for a in &verdict.actions {
                        self.metrics.plugin_action(name.as_str(), a.label());
                    }
                    outcome.actions.extend(verdict.actions);
                    self.metrics.intercept(
                        name.as_str(),
                        &event.name,
                        started.elapsed().as_secs_f64(),
                        None,
                    );
                }
                Err(e) => {
                    tracing::warn!(plugin = %name, agent = %agent, event = %event.name, "interceptor skipped: {e}");
                    self.metrics.intercept(
                        name.as_str(),
                        &event.name,
                        started.elapsed().as_secs_f64(),
                        Some(e.reason()),
                    );
                }
            }
        }
        for (name, _) in self.registry.observers(&agent, &event.name) {
            self.metrics
                .plugin_event(name.as_str(), &event.name, "observe");
            if self.queue_for(&name).is_some_and(|q| q.push(event.clone())) {
                self.metrics
                    .events_dropped(name.as_str(), DropReason::Overflow, 1);
            }
        }
        outcome
    }
}

impl EventHandler for PluginEventHandler {
    fn handle<'a>(&'a self, event: &'a HookEvent) -> HandlerFuture<'a> {
        Box::pin(self.run(event))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::registry::ActivationRow;
    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use balerix_api::{HookEvent, PluginAction, Timestamp};
    use balerix_core::ResolvedPlugin;
    use proptest::prelude::*;
    use serde_json::{Value, json};
    use std::sync::Mutex as StdMutex;

    fn plugin(name: &str, intercept: &[&str], observe: &[&str]) -> ResolvedPlugin {
        ResolvedPlugin {
            name: name.parse().unwrap(),
            package: format!("/pkg/{name}").into(),
            manifest: serde_json::from_value(json!({
                "apiVersion": "balerix/v1", "kind": "Plugin", "name": name,
                "version": "0.1.0", "protocol": 1, "start": "serve",
                "hooks": { "intercept": intercept, "observe": observe }
            }))
            .unwrap(),
            config: json!({}),
            fleet_defaults: json!({}),
            digest: None,
        }
    }

    fn event(name: &str) -> HookEvent {
        HookEvent {
            agent: "f/c/a".into(),
            name: name.into(),
            session_id: None,
            received_at: Timestamp(1),
            payload: json!({ "tool_input": { "command": "rm -rf /" } }),
        }
    }

    /// What one stub plugin does with an intercept.
    #[derive(Clone, Debug)]
    enum Behaviour {
        /// Merge `{ key: value }` over `response_so_far`, plus these actions.
        Merge(String, i64, Vec<PluginAction>),
        Status500,
        NotAnObject,
        Sleep(u64),
        /// Sleep this long, then merge `{ key: deadline_ms }`.
        Deadline(String, u64),
    }

    #[derive(Clone)]
    struct StubState {
        behaviour: Behaviour,
        batches: Arc<StdMutex<Vec<Vec<HookEvent>>>>,
        events_status: u16,
    }

    async fn stub(
        behaviour: Behaviour,
        events_status: u16,
    ) -> (String, Arc<StdMutex<Vec<Vec<HookEvent>>>>) {
        let batches = Arc::new(StdMutex::new(Vec::new()));
        let state = StubState {
            behaviour,
            batches: batches.clone(),
            events_status,
        };
        let app = Router::new()
            .route(
                "/v1/intercept",
                post(|State(s): State<StubState>, Json(v): Json<Value>| async move {
                    match s.behaviour {
                        Behaviour::Merge(k, n, actions) => {
                            let mut r = v["response_so_far"].clone();
                            r[k] = json!(n);
                            (
                                axum::http::StatusCode::OK,
                                Json(json!({ "response": r, "actions": actions })),
                            )
                        }
                        Behaviour::Status500 => (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": "boom" })),
                        ),
                        Behaviour::NotAnObject => {
                            (axum::http::StatusCode::OK, Json(json!({ "response": [1] })))
                        }
                        Behaviour::Sleep(ms) => {
                            tokio::time::sleep(Duration::from_millis(ms)).await;
                            (
                                axum::http::StatusCode::OK,
                                Json(json!({ "response": { "late": true } })),
                            )
                        }
                        Behaviour::Deadline(k, ms) => {
                            tokio::time::sleep(Duration::from_millis(ms)).await;
                            let mut r = v["response_so_far"].clone();
                            r[k] = v["deadline_ms"].clone();
                            (axum::http::StatusCode::OK, Json(json!({ "response": r })))
                        }
                    }
                }),
            )
            .route(
                "/v1/events",
                post(|State(s): State<StubState>, Json(b): Json<balerix_api::EventBatch>| async move {
                    s.batches.lock().unwrap().push(b.events);
                    axum::http::StatusCode::from_u16(s.events_status).unwrap()
                }),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr, batches)
    }

    /// An observer whose `/v1/events` records each batch, then holds its
    /// answer (`status`) until the test adds a permit to the gate: while
    /// one batch is held, the delivery task is stuck in that call and
    /// everything pushed after it stays queued.
    async fn gated_stub(
        status: u16,
    ) -> (
        String,
        Arc<StdMutex<Vec<Vec<HookEvent>>>>,
        Arc<tokio::sync::Semaphore>,
    ) {
        let batches = Arc::new(StdMutex::new(Vec::new()));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let app = Router::new().route(
            "/v1/events",
            post({
                let batches = batches.clone();
                let gate = gate.clone();
                move |Json(b): Json<balerix_api::EventBatch>| async move {
                    batches.lock().unwrap().push(b.events);
                    gate.acquire().await.unwrap().forget();
                    axum::http::StatusCode::from_u16(status).unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr, batches, gate)
    }

    async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !cond() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    /// One observer `web` of `Stop` behind a gated stub, with one batch of
    /// one event already held there.
    async fn held_observer(
        status: u16,
    ) -> (
        Arc<PluginRegistry>,
        Arc<PluginEventHandler>,
        Metrics,
        Arc<StdMutex<Vec<Vec<HookEvent>>>>,
        Arc<tokio::sync::Semaphore>,
    ) {
        let r = PluginRegistry::new();
        r.replace_plugins(&[plugin("web", &[], &["Stop"])], &[]);
        let (listen, batches, gate) = gated_stub(status).await;
        r.set_listen(&"web".parse().unwrap(), listen, "t".into());
        active(&r, "web");
        let (h, m) = handler(&r);
        h.handle(&event("Stop")).await;
        let b = batches.clone();
        wait_until("the first batch", || b.lock().unwrap().len() == 1).await;
        (r, h, m, batches, gate)
    }

    fn active(r: &PluginRegistry, plugin: &str) {
        r.set_row(
            &"f/c/a".parse().unwrap(),
            &plugin.parse().unwrap(),
            ActivationRow {
                config: json!({}),
                activation: balerix_api::PluginActivation::active(),
            },
        );
    }

    fn handler(r: &Arc<PluginRegistry>) -> (Arc<PluginEventHandler>, Metrics) {
        let m = Metrics::new().unwrap();
        (
            PluginEventHandler::new(r.clone(), PluginClient::new(None).unwrap(), m.clone()),
            m,
        )
    }

    #[tokio::test]
    async fn the_chain_folds_verdicts_in_load_order_and_skips_failures() {
        let r = PluginRegistry::new();
        r.replace_plugins(
            &[
                plugin("first", &["PreToolUse"], &[]),
                plugin("broken", &["PreToolUse"], &[]),
                plugin("odd", &["PreToolUse"], &[]),
                plugin("last", &["PreToolUse"], &[]),
                plugin("bystander", &["Stop"], &[]),
            ],
            &[],
        );
        let (first, _) = stub(
            Behaviour::Merge("a".into(), 1, vec![PluginAction::Stop]),
            200,
        )
        .await;
        let (broken, _) = stub(Behaviour::Status500, 200).await;
        let (odd, _) = stub(Behaviour::NotAnObject, 200).await;
        let (last, _) = stub(
            Behaviour::Merge(
                "b".into(),
                2,
                vec![PluginAction::SendText {
                    text: "hi".into(),
                    submit: true,
                }],
            ),
            200,
        )
        .await;
        for (n, l) in [
            ("first", first),
            ("broken", broken),
            ("odd", odd),
            ("last", last),
        ] {
            r.set_listen(&n.parse().unwrap(), l, "t".into());
            active(&r, n);
        }
        let (h, m) = handler(&r);
        let out = h.handle(&event("PreToolUse")).await;
        assert_eq!(out.response, json!({ "a": 1, "b": 2 }));
        assert_eq!(
            out.actions,
            vec![
                PluginAction::Stop,
                PluginAction::SendText {
                    text: "hi".into(),
                    submit: true
                }
            ]
        );
        let text = m.encode();
        assert!(
            text.contains(
                "balerix_plugin_intercept_failures_total{plugin=\"broken\",reason=\"status\"} 1"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "balerix_plugin_intercept_failures_total{plugin=\"odd\",reason=\"body\"} 1"
            )
        );
        assert!(text.contains(
            "balerix_plugin_events_total{event=\"PreToolUse\",mode=\"intercept\",plugin=\"first\"} 1"
        ));
        assert!(
            text.contains("balerix_plugin_actions_total{action=\"stop\",plugin=\"first\"} 1"),
            "{text}"
        );
        assert!(
            !text.contains("plugin=\"bystander\""),
            "not subscribed to this event"
        );
        // nobody intercepts Notification: allow, and cheap
        assert_eq!(h.handle(&event("Notification")).await, Outcome::allow());
    }

    #[tokio::test]
    async fn a_slow_plugin_is_skipped_within_the_budget_and_a_dead_one_counts_as_connect() {
        let r = PluginRegistry::new();
        r.replace_plugins(
            &[
                plugin("dead", &["PreToolUse"], &[]),
                plugin("slow", &["PreToolUse"], &[]),
                plugin("ok", &["PreToolUse"], &[]),
            ],
            &[],
        );
        let (slow, _) = stub(Behaviour::Sleep(5_000), 200).await;
        let (ok, _) = stub(Behaviour::Merge("k".into(), 9, vec![]), 200).await;
        r.set_listen(&"dead".parse().unwrap(), "127.0.0.1:1".into(), "t".into());
        r.set_listen(&"slow".parse().unwrap(), slow, "t".into());
        r.set_listen(&"ok".parse().unwrap(), ok, "t".into());
        for n in ["dead", "slow", "ok"] {
            active(&r, n);
        }
        let (h, m) = handler(&r);
        let started = Instant::now();
        let out = h.handle(&event("PreToolUse")).await;
        let took = started.elapsed();
        assert!(took < Duration::from_millis(1900), "chain took {took:?}");
        assert!(
            took >= Duration::from_millis(1400),
            "the slow plugin got the whole budget: {took:?}"
        );
        assert_eq!(
            out.response,
            json!({}),
            "the budget was spent before ok ran"
        );
        let text = m.encode();
        assert!(
            text.contains(
                "balerix_plugin_intercept_failures_total{plugin=\"slow\",reason=\"timeout\"} 1"
            ),
            "{text}"
        );
        assert!(text.contains(
            "balerix_plugin_intercept_failures_total{plugin=\"dead\",reason=\"connect\"} 1"
        ));
        assert!(
            text.contains(
                "balerix_plugin_intercept_failures_total{plugin=\"ok\",reason=\"timeout\"} 1"
            ),
            "no budget left: skipped as a timeout"
        );
    }

    #[tokio::test]
    async fn observers_get_batches_in_order_and_hello_clears_the_queue() {
        let r = PluginRegistry::new();
        r.replace_plugins(&[plugin("web", &[], &["Stop", "Notification"])], &[]);
        let (listen, batches) = stub(Behaviour::Status500, 200).await;
        r.set_listen(&"web".parse().unwrap(), listen, "t".into());
        active(&r, "web");
        let (h, _m) = handler(&r);
        for i in 0..70 {
            let mut e = event("Stop");
            e.payload = json!({ "i": i });
            assert_eq!(
                h.handle(&e).await,
                Outcome::allow(),
                "observers never block or answer"
            );
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let got: usize = batches.lock().unwrap().iter().map(Vec::len).sum();
                if got == 70 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("all 70 delivered");
        {
            let b = batches.lock().unwrap();
            assert!(b[0].len() <= OBSERVER_BATCH);
            let order: Vec<i64> = b
                .iter()
                .flatten()
                .map(|e| e.payload["i"].as_i64().unwrap())
                .collect();
            assert_eq!(
                order,
                (0..70).collect::<Vec<_>>(),
                "order kept across batches"
            );
        }
        // not ready: nothing queued
        r.set_ready(&"web".parse().unwrap(), false);
        h.handle(&event("Notification")).await;
        assert_eq!(h.queue_len(&"web".parse().unwrap()), 0);
    }

    /// #13: what the delivery task has not sent yet is gone after a
    /// `hello` (§4.3: no catch-up).
    #[tokio::test]
    async fn hello_clears_what_the_delivery_task_has_not_sent() {
        let (_r, h, _m, batches, gate) = held_observer(200).await;
        let web: AgentName = "web".parse().unwrap();
        for _ in 0..3 {
            h.handle(&event("Stop")).await;
        }
        assert_eq!(h.queue_len(&web), 3, "the task is held in the first call");
        h.on_hello(&web);
        assert_eq!(h.queue_len(&web), 0);
        gate.add_permits(10);
        tokio::time::sleep(BATCH_WINDOW * 3).await;
        assert_eq!(
            batches.lock().unwrap().len(),
            1,
            "nothing queued before the hello was sent"
        );
    }

    /// #13: each interceptor is told what remains of the shared budget,
    /// not the whole of it.
    #[tokio::test]
    async fn deadline_ms_is_what_remains_of_the_budget() {
        let r = PluginRegistry::new();
        r.replace_plugins(
            &[
                plugin("first", &["PreToolUse"], &[]),
                plugin("second", &["PreToolUse"], &[]),
            ],
            &[],
        );
        let (first, _) = stub(Behaviour::Deadline("first".into(), 300), 200).await;
        let (second, _) = stub(Behaviour::Deadline("second".into(), 0), 200).await;
        r.set_listen(&"first".parse().unwrap(), first, "t".into());
        r.set_listen(&"second".parse().unwrap(), second, "t".into());
        active(&r, "first");
        active(&r, "second");
        let (h, _m) = handler(&r);
        let out = h.handle(&event("PreToolUse")).await;
        let first = out.response["first"].as_u64().unwrap();
        let second = out.response["second"].as_u64().unwrap();
        assert!(
            first <= CHAIN_BUDGET_MS && first > CHAIN_BUDGET_MS - 500,
            "first: {first}"
        );
        assert!(
            second <= first - 300 && second > 0,
            "second: {second} after first's 300 ms of {first}"
        );
    }

    /// #13: an observer that cannot keep up loses its oldest events, and
    /// the handler counts them as `overflow`.
    #[tokio::test]
    async fn the_handler_counts_an_overflow_drop() {
        let (_r, h, m, _batches, _gate) = held_observer(200).await;
        for _ in 0..(OBSERVER_QUEUE + 5) {
            h.handle(&event("Stop")).await;
        }
        assert_eq!(h.queue_len(&"web".parse().unwrap()), OBSERVER_QUEUE);
        let text = m.encode();
        assert!(
            text.contains(
                "balerix_plugin_events_dropped_total{plugin=\"web\",reason=\"overflow\"} 5"
            ),
            "{text}"
        );
    }

    /// #13: a batch that is ready to send while its plugin is not is
    /// dropped whole, as `not_ready`.
    #[tokio::test]
    async fn a_batch_ready_while_the_plugin_is_not_is_dropped_as_not_ready() {
        let (r, h, m, batches, gate) = held_observer(200).await;
        for _ in 0..4 {
            h.handle(&event("Stop")).await;
        }
        r.set_ready(&"web".parse().unwrap(), false);
        gate.add_permits(10);
        let m2 = m.clone();
        wait_until("the not-ready drop", || {
            m2.encode().contains(
                "balerix_plugin_events_dropped_total{plugin=\"web\",reason=\"not_ready\"} 4",
            )
        })
        .await;
        assert_eq!(batches.lock().unwrap().len(), 1, "nothing more was sent");
    }

    /// #13: a batch the plugin does not acknowledge counts as
    /// `unacknowledged`.
    #[tokio::test]
    async fn an_unacknowledged_batch_is_counted() {
        let (_r, _h, m, _batches, gate) = held_observer(500).await;
        gate.add_permits(1);
        let m2 = m.clone();
        wait_until("the unacknowledged drop", || {
            m2.encode().contains(
                "balerix_plugin_events_dropped_total{plugin=\"web\",reason=\"unacknowledged\"} 1",
            )
        })
        .await;
    }

    /// #13: a removed plugin's delivery task ends and its queue goes.
    #[tokio::test]
    async fn remove_ends_the_delivery_task_and_drops_the_queue() {
        let (_r, h, _m, _batches, _gate) = held_observer(200).await;
        let web: AgentName = "web".parse().unwrap();
        h.handle(&event("Stop")).await;
        let q = h.queue_for(&web).unwrap();
        assert_eq!(q.len(), 1);
        h.remove(&web);
        assert_eq!(h.queue_len(&web), 0);
        wait_until("the delivery task to let go of the queue", || {
            Arc::strong_count(&q) == 1
        })
        .await;
    }

    /// #13: an event that read the observers before a sync removed the
    /// plugin makes no queue for it after `remove` ran.
    #[tokio::test]
    async fn no_queue_is_made_for_a_plugin_the_registry_dropped() {
        let (r, h, _m, _batches, _gate) = held_observer(200).await;
        let web: AgentName = "web".parse().unwrap();
        r.replace_plugins(&[], &[]);
        h.remove(&web);
        assert!(h.queue_for(&web).is_none());
        assert_eq!(h.queue_len(&web), 0);
    }

    #[test]
    fn the_queue_drops_the_oldest_on_overflow() {
        let q = ObserverQueue::new("web".parse().unwrap());
        for i in 0..(OBSERVER_QUEUE as i64 + 5) {
            let mut e = event("Stop");
            e.payload = json!({ "i": i });
            let dropped = q.push(e);
            assert_eq!(dropped, i >= OBSERVER_QUEUE as i64, "i={i}");
        }
        assert_eq!(q.len(), OBSERVER_QUEUE);
        let first = q.drain(1);
        assert_eq!(first[0].payload["i"], 5, "the five oldest went");
        assert_eq!(q.drain(OBSERVER_BATCH).len(), OBSERVER_BATCH);
        q.clear();
        assert_eq!(q.len(), 0);
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]
        /// Spec §11 property: the final response is the fold over the
        /// plugins that did not fail, whichever ones do.
        #[test]
        fn the_final_response_is_the_fold_over_the_non_failing_plugins(
            fails in proptest::collection::vec(any::<bool>(), 1..5)
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let r = PluginRegistry::new();
                let names: Vec<String> = (0..fails.len()).map(|i| format!("p{i}")).collect();
                let plugins: Vec<ResolvedPlugin> = names.iter().map(|n| plugin(n, &["PreToolUse"], &[])).collect();
                r.replace_plugins(&plugins, &[]);
                let mut expected = serde_json::Map::new();
                for (i, (name, fail)) in names.iter().zip(&fails).enumerate() {
                    let behaviour = if *fail { Behaviour::Status500 } else { Behaviour::Merge(name.clone(), i as i64, vec![]) };
                    let (listen, _) = stub(behaviour, 200).await;
                    r.set_listen(&name.parse().unwrap(), listen, "t".into());
                    active(&r, name);
                    if !fail {
                        expected.insert(name.clone(), json!(i));
                    }
                }
                let (h, _) = handler(&r);
                let out = h.handle(&event("PreToolUse")).await;
                assert_eq!(out.response, Value::Object(expected));
            });
        }
    }
}
