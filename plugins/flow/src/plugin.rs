//! The `Plugin` impl (plugins spec §17.3, §17.4): one entry per active
//! agent, the current state mirrored to the daemon's KV under
//! `state/<agent>` so a restart resumes, and the metric families.
//!
//! A `send` with `submit: true` is a prompt the daemon types and submits,
//! and `SessionStart` is the natural place to send the first one. Text
//! that reaches the pane while Claude Code's TUI is still starting lands
//! in the composer and loses its Enter (#100). So every submitted send is
//! registered with common's delivery tracker (Spec M §8.7), the agent's
//! own `UserPromptSubmit` confirms it, and until then [`Nudger::tick`]
//! presses Enter again on the tracker's cadence. Flow has nobody to
//! report an expiry to, so a prompt's window is zero: confirmation and
//! the tracker's own nudge bound are all that settle it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use balerix_api::{DEFAULT_KEY_DELAY_MS, HookEvent, InterceptResponse, Key, KeyStep, PluginAction};
use balerix_plugin_common::delivery::Deliveries;
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
    metrics: Metrics,
    state_gauge: IntGaugeVec,
    transitions: IntCounterVec,
    nudger: Nudger,
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
            metrics,
            state_gauge,
            transitions,
        })
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
    /// to KV before the verdict returns. A KV failure is logged and the
    /// in-memory state stands — a hook never fails on it (§17.3).
    async fn intercept(
        &self,
        event: HookEvent,
        so_far: Value,
        _deadline_ms: u64,
    ) -> InterceptResponse {
        let agent = event.agent.clone();
        let now = Instant::now();
        self.nudger.on_event(&event, now);
        let (outcome, transition) = {
            let mut agents = self.agents.lock().unwrap_or_else(|e| e.into_inner());
            let Some(flow) = agents.get_mut(&agent) else {
                return InterceptResponse {
                    response: so_far,
                    actions: Vec::new(),
                };
            };
            let s = step(&flow.compiled, &flow.state, &event, so_far);
            for action in &s.actions {
                if let PluginAction::SendText { text, submit: true } = action {
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
