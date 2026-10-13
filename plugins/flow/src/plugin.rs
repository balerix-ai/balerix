//! The `Plugin` impl (plugins spec §17.3, §17.4): one entry per active
//! agent, the current state mirrored to the daemon's KV under
//! `state/<agent>` so a restart resumes, and the metric families.
//!
//! A `send` with `submit: true` is a prompt the daemon types and submits,
//! and `SessionStart` is the natural place to send the first one. Text
//! that reaches the pane while Claude Code's TUI is still starting lands
//! in the composer and loses its Enter (#100). So every submitted send is
//! registered with common's delivery tracker (Spec M §8.7), unless it
//! is a slash command, which no `UserPromptSubmit` follows (#255); the agent's
//! own `UserPromptSubmit` confirms it, and until then [`Nudger::tick`]
//! presses Enter again on the tracker's cadence. Flow has nobody to
//! report an expiry to, so a prompt's window is zero: confirmation and
//! the tracker's own nudge bound are all that settle it.
//!
//! A state change and its KV write happen under the agent's gate, an
//! async lock `activate` and `deactivate` take too, so a `deactivate`
//! never lands between an intercept's transition and its store: the
//! write cannot bring a dropped agent's state back, nor its gauge
//! series (#256).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use balerix_api::{DEFAULT_KEY_DELAY_MS, HookEvent, InterceptResponse, Key, KeyStep, PluginAction};
use balerix_plugin_common::delivery::{self, Deliveries, Kind};
use balerix_plugin_sdk::metrics::{IntCounterVec, IntGaugeVec};
use balerix_plugin_sdk::{Host, Metrics, Plugin, SdkError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::Instant;

use crate::config::{Compiled, compile};
use crate::machine::step;

/// The KV document under `state/<agent>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stored {
    pub state: String,
    /// `config_hash` of the config the state belongs to.
    pub config: String,
}

struct AgentFlow {
    compiled: Arc<Compiled>,
    state: String,
}

pub struct FlowPlugin {
    host: Host,
    agents: Mutex<HashMap<String, AgentFlow>>,
    /// Per agent, held across a state change and its KV write (#256).
    /// Made by the first `activate` and kept: a gate dropped on
    /// `deactivate` while an intercept still waited on it would let a
    /// later `activate` take a fresh one alongside. One entry per agent
    /// ever activated.
    gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    metrics: Metrics,
    state_gauge: IntGaugeVec,
    transitions: IntCounterVec,
    nudger: Nudger,
    #[cfg(test)]
    pause: Mutex<Option<Arc<tests::Pause>>>,
}

/// The half of the plugin that runs on a timer: the submitted sends not
/// yet confirmed, and the Enter pressed again for them. Cloned out of
/// the plugin before `serve` takes it, and ticked from `main`.
#[derive(Clone)]
pub struct Nudger {
    host: Host,
    deliveries: Arc<Mutex<Deliveries<()>>>,
    /// `deliveries_total{outcome}`: `confirmed`, `skipped`, `evicted`, `nudged`.
    deliveries_total: IntCounterVec,
    /// `errors_total{kind}`: `send_keys`, an Enter the daemon refused.
    errors: IntCounterVec,
}

impl Nudger {
    fn deliveries(&self) -> std::sync::MutexGuard<'_, Deliveries<()>> {
        self.deliveries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Presses Enter once on every agent the tracker says is due at
    /// `now` (#100): idle, holding an unconfirmed prompt, and 5 s past
    /// its clock or its last press.
    pub async fn tick(&self, now: Instant) {
        let due = self.deliveries().nudge(now);
        for agent in due {
            let action = PluginAction::SendKeys {
                steps: vec![KeyStep::Key(Key::Enter)],
                delay_ms: DEFAULT_KEY_DELAY_MS,
            };
            match self.host.action(&agent, &action).await {
                Ok(()) => self.deliveries_total.with_label_values(&["nudged"]).inc(),
                Err(e) => {
                    self.errors.with_label_values(&["send_keys"]).inc();
                    eprintln!("flow: Enter again for {agent}: {e}");
                }
            }
        }
    }

    /// One hook event to the tracker: what it confirms or skips is counted.
    fn on_event(&self, event: &HookEvent, now: Instant) {
        let outcome = self.deliveries().on_event(event, now);
        if outcome.confirmed.is_some() {
            self.deliveries_total
                .with_label_values(&["confirmed"])
                .inc();
        }
        self.deliveries_total
            .with_label_values(&["skipped"])
            .inc_by(outcome.skipped.len() as u64);
    }

    /// A submitted send just returned in a verdict, with a zero window:
    /// flow reports no expiry, so only confirmation settles it.
    fn sent(&self, agent: &str, text: &str, now: Instant) {
        let evicted = self.deliveries().sent(agent, text, (), now, Duration::ZERO);
        if evicted.is_some() {
            self.deliveries_total.with_label_values(&["evicted"]).inc();
        }
    }

    fn forget(&self, agent: &str) {
        self.deliveries().forget(agent);
    }
}

impl std::fmt::Debug for FlowPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlowPlugin")
            .field("host", &self.host)
            .finish()
    }
}

/// How often `main` ticks the [`Nudger`]: the tracker's own cadence.
pub const TICK: Duration = balerix_plugin_common::delivery::NUDGE_AFTER;

const STATE_LABELS: [&str; 4] = ["fleet", "crew", "agent", "state"];
const TRANSITION_LABELS: [&str; 5] = ["fleet", "crew", "agent", "from", "to"];

impl FlowPlugin {
    pub fn new(host: Host) -> Result<Self, SdkError> {
        let metrics = Metrics::new(&host.env().name);
        let state_gauge = metrics.int_gauge_vec(
            "state",
            "Current flow state (1 for the current state)",
            &STATE_LABELS,
        )?;
        let transitions = metrics.int_counter_vec(
            "transitions_total",
            "Flow transitions since the plugin started",
            &TRANSITION_LABELS,
        )?;
        let deliveries_total = metrics.int_counter_vec(
            "deliveries_total",
            "Submitted sends by what settled them (#100)",
            &["outcome"],
        )?;
        let errors = metrics.int_counter_vec("errors_total", "Failures by kind", &["kind"])?;
        Ok(Self {
            nudger: Nudger {
                host: host.clone(),
                deliveries: Arc::new(Mutex::new(Deliveries::default())),
                deliveries_total,
                errors,
            },
            host,
            agents: Mutex::new(HashMap::new()),
            gates: Mutex::new(HashMap::new()),
            metrics,
            state_gauge,
            transitions,
            #[cfg(test)]
            pause: Mutex::new(None),
        })
    }

    fn gates(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<tokio::sync::Mutex<()>>>> {
        self.gates.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `agent`'s gate, made if it has none yet.
    fn gate(&self, agent: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.gates().entry(agent.to_string()).or_default().clone()
    }

    /// `agent`'s gate, if it was ever activated.
    fn existing_gate(&self, agent: &str) -> Option<Arc<tokio::sync::Mutex<()>>> {
        self.gates().get(agent).cloned()
    }

    /// The timer half; see [`Nudger`].
    pub fn nudger(&self) -> Nudger {
        self.nudger.clone()
    }

    pub fn state_key(agent: &str) -> String {
        format!("state/{agent}")
    }

    /// `fleet/crew/agent` → the three labels; an id of another shape
    /// (never produced by the daemon) keeps its text in `agent`.
    fn labels(agent: &str) -> [String; 3] {
        let mut parts = agent.splitn(3, '/');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(f), Some(c), Some(a)) => [f.into(), c.into(), a.into()],
            _ => [String::new(), String::new(), agent.into()],
        }
    }

    fn set_state_gauge(&self, agent: &str, from: Option<&str>, to: &str) {
        let [f, c, a] = Self::labels(agent);
        if let Some(from) = from {
            // Err only when the series was never set; nothing to do then.
            let _ = self.state_gauge.remove_label_values(&[&f, &c, &a, from]);
        }
        self.state_gauge.with_label_values(&[&f, &c, &a, to]).set(1);
    }

    fn count_transition(&self, agent: &str, from: &str, to: &str) {
        let [f, c, a] = Self::labels(agent);
        self.transitions
            .with_label_values(&[&f, &c, &a, from, to])
            .inc();
    }

    async fn store(&self, agent: &str, stored: &Stored) -> Result<(), SdkError> {
        let bytes = serde_json::to_vec(stored)
            .map_err(|e| SdkError::Transport(format!("encode state: {e}")))?;
        #[cfg(test)]
        tests::Pause::at_store(&self.pause).await;
        self.host
            .kv_put(&Self::state_key(agent), &bytes, false)
            .await
    }
}

impl Plugin for FlowPlugin {
    fn manifest(&self) -> Option<&'static str> {
        Some(include_str!("../package/balerix-plugin.yaml"))
    }

    /// Compile, then resume the stored state when it belongs to this very
    /// config and is still declared; otherwise start at `initial` and
    /// store that. A KV fault rejects: the operator's `up` should fail
    /// loudly on a daemon-side error (§17.3).
    async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
        let compiled = Arc::new(compile(&config).map_err(|e| e.to_string())?);
        let gate = self.gate(agent);
        let _held = gate.lock().await;
        let key = Self::state_key(agent);
        let previous = self
            .host
            .kv_get(&key)
            .await
            .map_err(|e| format!("kv: {e}"))?
            .and_then(|bytes| {
                serde_json::from_slice::<Stored>(&bytes)
                    .map_err(|e| eprintln!("flow: bad stored state for {agent}: {e}"))
                    .ok()
            });
        let resumed = previous
            .filter(|s| s.config == compiled.hash && compiled.states.contains_key(&s.state))
            .map(|s| s.state);
        let state = match resumed {
            Some(state) => state,
            None => {
                let stored = Stored {
                    state: compiled.initial.clone(),
                    config: compiled.hash.clone(),
                };
                self.store(agent, &stored)
                    .await
                    .map_err(|e| format!("kv: {e}"))?;
                stored.state
            }
        };
        let old = {
            let mut agents = self.agents.lock().unwrap_or_else(|e| e.into_inner());
            agents
                .insert(
                    agent.to_string(),
                    AgentFlow {
                        compiled,
                        state: state.clone(),
                    },
                )
                .map(|a| a.state)
        };
        self.set_state_gauge(agent, old.as_deref(), &state);
        eprintln!("flow: {agent} active in state {state:?}");
        Ok(())
    }

    /// Forget the agent and its stored state: `down`, a dropped block and
    /// a config change all reset to `initial` on the next `activate`.
    async fn deactivate(&self, agent: &str) {
        let gate = self.existing_gate(agent);
        let _held = match &gate {
            Some(gate) => Some(gate.lock().await),
            None => None,
        };
        self.nudger.forget(agent);
        let old = self
            .agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(agent)
            .map(|a| a.state);
        if let Some(old) = old {
            let [f, c, a] = Self::labels(agent);
            let _ = self.state_gauge.remove_label_values(&[&f, &c, &a, &old]);
        }
        if let Err(e) = self.host.kv_delete(&Self::state_key(agent)).await {
            eprintln!("flow: kv delete for {agent}: {e}");
        }
    }

    /// One step; the transition is applied in memory first, then written
    /// to KV before the verdict returns, both under the agent's gate. A
    /// KV failure is logged and the in-memory state stands — a hook never
    /// fails on it (§17.3).
    async fn intercept(
        &self,
        event: HookEvent,
        so_far: Value,
        _deadline_ms: u64,
    ) -> InterceptResponse {
        let agent = event.agent.clone();
        let now = Instant::now();
        self.nudger.on_event(&event, now);
        let pass = |so_far| InterceptResponse {
            response: so_far,
            actions: Vec::new(),
        };
        let Some(gate) = self.existing_gate(&agent) else {
            return pass(so_far);
        };
        let _held = gate.lock().await;
        let (outcome, transition) = {
            let mut agents = self.agents.lock().unwrap_or_else(|e| e.into_inner());
            let Some(flow) = agents.get_mut(&agent) else {
                return pass(so_far);
            };
            let s = step(&flow.compiled, &flow.state, &event, so_far);
            for action in &s.actions {
                // A slash command gets no `UserPromptSubmit`, so it is never
                // tracked: nothing could confirm it (#255).
                if let PluginAction::SendText { text, submit: true } = action
                    && delivery::classify(text) == Kind::Prompt
                {
                    self.nudger.sent(&agent, text, now);
                }
            }
            let transition = s.next.clone().map(|to| {
                let from = std::mem::replace(&mut flow.state, to.clone());
                (from, to, flow.compiled.hash.clone())
            });
            (s, transition)
        };
        if let Some((from, to, hash)) = transition {
            self.count_transition(&agent, &from, &to);
            self.set_state_gauge(&agent, Some(&from), &to);
            eprintln!("flow: {agent} {from} -> {to} on {}", event.name);
            let stored = Stored {
                state: to,
                config: hash,
            };
            if let Err(e) = self.store(&agent, &stored).await {
                eprintln!("flow: kv put for {agent}: {e}");
            }
        }
        InterceptResponse {
            response: outcome.response,
            actions: outcome.actions,
        }
    }

    fn metrics(&self) -> Option<&Metrics> {
        Some(&self.metrics)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use balerix_plugin_sdk::testing::{FakeHost, event, metric};
    use serde_json::json;
    use tokio::sync::Notify;

    const ALICE: &str = "e2e/c/alice";

    /// Holds the next `store` open: says when it is reached, waits to be
    /// released.
    #[derive(Default)]
    pub(super) struct Pause {
        reached: Notify,
        release: Notify,
    }

    impl Pause {
        pub(super) async fn at_store(slot: &Mutex<Option<Arc<Pause>>>) {
            let pause = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(pause) = pause {
                pause.reached.notify_one();
                pause.release.notified().await;
            }
        }
    }

    fn config() -> Value {
        json!({
            "initial": "working",
            "states": {
                "working": { "on": [ { "event": "Stop", "goto": "review" } ] },
                "review": {}
            }
        })
    }

    /// #256: a `deactivate` that arrives while an intercept is storing
    /// its transition waits for it, then deletes: no state is left
    /// behind, no gauge series, and the next `activate` starts at
    /// `initial`.
    #[tokio::test]
    async fn deactivate_during_a_store_leaves_no_state_behind() {
        let fake = FakeHost::start("tok", json!({}), vec![]).await;
        let env = fake.env("flow", std::path::Path::new("/s"));
        let plugin = FlowPlugin::new(Host::new(env).unwrap()).unwrap();
        plugin.activate(ALICE, config()).await.unwrap();

        let pause = Arc::new(Pause::default());
        *plugin.pause.lock().unwrap() = Some(pause.clone());
        let stop = event(ALICE, "Stop", json!({}));
        let (verdict, waited) = tokio::join!(plugin.intercept(stop, json!({}), 900), async {
            pause.reached.notified().await; // working -> review, not yet stored
            let deactivate = plugin.deactivate(ALICE);
            tokio::pin!(deactivate);
            let early = tokio::time::timeout(Duration::from_millis(200), &mut deactivate).await;
            pause.release.notify_one();
            if early.is_err() {
                deactivate.await;
            }
            early.is_err()
        });
        assert!(verdict.actions.is_empty());
        assert!(waited, "deactivate waited for the store");
        assert_eq!(fake.kv_json(&FlowPlugin::state_key(ALICE)), None);
        let m = plugin.metrics.render().unwrap();
        assert_eq!(
            metric(&m, "balerix_plugin_flow_state", &[("agent", "alice")]),
            None,
            "{m}"
        );

        plugin.activate(ALICE, config()).await.unwrap();
        assert_eq!(
            fake.kv_json(&FlowPlugin::state_key(ALICE)).unwrap()["state"],
            "working"
        );
    }
}
