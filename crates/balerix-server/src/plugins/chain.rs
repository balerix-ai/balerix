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
use crate::metrics::Metrics;

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
                metrics.events_dropped(self.plugin.as_str(), batch.len() as u64);
                continue;
            };
            let n = batch.len();
            if let Err(e) = client.events(&addr, &EventBatch { events: batch }).await {
                tracing::warn!(plugin = %self.plugin, events = n, "observer batch not acknowledged: {e}");
                metrics.events_dropped(self.plugin.as_str(), n as u64);
            }
        }
    }
}

pub struct PluginEventHandler {
    registry: Arc<PluginRegistry>,
    client: PluginClient,
    metrics: Metrics,
    observers: Mutex<BTreeMap<AgentName, Arc<ObserverQueue>>>,
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
    fn queue_for(&self, name: &AgentName) -> Arc<ObserverQueue> {
        let mut map = lock(&self.observers);
        if let Some(q) = map.get(name) {
            return q.clone();
        }
        let q = ObserverQueue::new(name.clone());
        tokio::spawn(q.clone().deliver(
            self.registry.clone(),
            self.client.clone(),
            self.metrics.clone(),
        ));
        map.insert(name.clone(), q.clone());
        q
    }

    /// A plugin that just said hello starts from an empty queue (§4.3).
    pub fn on_hello(&self, name: &AgentName) {
        if let Some(q) = lock(&self.observers).get(name) {
            q.clear();
        }
    }

    pub fn queue_len(&self, name: &AgentName) -> usize {
        lock(&self.observers).get(name).map_or(0, |q| q.len())
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
            if self.queue_for(&name).push(event.clone()) {
                self.metrics.events_dropped(name.as_str(), 1);
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
            PluginEventHandler::new(r.clone(), PluginClient::new().unwrap(), m.clone()),
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
        // not ready: nothing queued; hello clears whatever was
        r.set_ready(&"web".parse().unwrap(), false);
        h.handle(&event("Notification")).await;
        assert_eq!(h.queue_len(&"web".parse().unwrap()), 0);
        h.on_hello(&"web".parse().unwrap());
        assert_eq!(h.queue_len(&"web".parse().unwrap()), 0);
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
