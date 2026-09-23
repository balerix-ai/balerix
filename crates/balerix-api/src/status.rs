//! Fleet and agent status (spec §7 status model; Phase 2 spec §2.3). Wire
//! types: the daemon stores and returns these.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Seconds since the Unix epoch. Whole seconds are enough for backoff and
/// resync decisions and keep the wire form a plain integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Timestamp(pub u64);

impl Timestamp {
    pub fn plus_secs(self, s: u64) -> Self {
        Self(self.0.saturating_add(s))
    }
}

/// Hex SHA-256 of an agent's resolved spec. Computed in `balerix-core`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SpecHash(String);

impl SpecHash {
    pub fn new(hex: String) -> Self {
        Self(hex)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FleetPhase {
    Pending,
    Reconciling,
    Ready,
    Degraded,
    Terminating,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentPhase {
    Pending,
    Materializing,
    Starting,
    Ready,
    Dead,
    Stopped,
}

/// Where a plugin stands for one agent (plugins spec §16.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivationState {
    /// Recorded; `activate` goes out at the plugin's next `hello`.
    Pending,
    /// The plugin accepted the agent's config.
    Active,
    /// The plugin refused; `message` is its error.
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginActivation {
    pub state: ActivationState,
    #[serde(default)]
    pub message: String,
}

impl PluginActivation {
    pub fn pending() -> Self {
        Self {
            state: ActivationState::Pending,
            message: String::new(),
        }
    }
    pub fn active() -> Self {
        Self {
            state: ActivationState::Active,
            message: String::new(),
        }
    }
    pub fn rejected(message: impl Into<String>) -> Self {
        Self {
            state: ActivationState::Rejected,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub phase: AgentPhase,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub last_event_at: Option<Timestamp>,
    #[serde(default)]
    pub applied_hash: Option<SpecHash>,
    #[serde(default)]
    pub restarts: u32,
    #[serde(default)]
    pub next_restart_at: Option<Timestamp>,
    /// Plugin name → activation state, filled in by the daemon on the way
    /// out (a read-time overlay); never stored.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, PluginActivation>,
}

impl Default for AgentStatus {
    fn default() -> Self {
        Self {
            phase: AgentPhase::Pending,
            message: String::new(),
            last_event_at: None,
            applied_hash: None,
            restarts: 0,
            next_restart_at: None,
            plugins: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetStatus {
    pub generation: u64,
    pub observed_generation: u64,
    pub phase: FleetPhase,
    /// Keyed by `AgentId` display form `fleet/crew/agent`.
    #[serde(default)]
    pub agents: BTreeMap<String, AgentStatus>,
}

impl Default for FleetStatus {
    fn default() -> Self {
        Self {
            generation: 0,
            observed_generation: 0,
            phase: FleetPhase::Pending,
            agents: BTreeMap::new(),
        }
    }
}

impl FleetStatus {
    /// The status entry for `id`, created as `Pending` if absent.
    pub fn entry(&mut self, id: &str) -> &mut AgentStatus {
        self.agents.entry(id.to_string()).or_default()
    }
}

/// One row of `GET /v1/fleets`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetSummary {
    pub name: String,
    pub phase: FleetPhase,
    pub generation: u64,
    pub observed_generation: u64,
    pub agents: usize,
    /// `FleetRecord::owner` (Spec L §5): the plugin that manages the fleet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_by: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn timestamp_addition_saturates() {
        assert_eq!(Timestamp(10).plus_secs(5), Timestamp(15));
        assert_eq!(Timestamp(u64::MAX).plus_secs(5), Timestamp(u64::MAX));
    }

    #[test]
    fn phases_serialize_lowercase() {
        assert_eq!(
            serde_json::to_value(FleetPhase::Degraded).unwrap(),
            json!("degraded")
        );
        assert_eq!(
            serde_json::to_value(AgentPhase::Materializing).unwrap(),
            json!("materializing")
        );
    }

    #[test]
    fn entry_inserts_a_pending_default() {
        let mut s = FleetStatus::default();
        s.entry("f/c/a").restarts = 3;
        assert_eq!(s.agents["f/c/a"].phase, AgentPhase::Pending);
        assert_eq!(s.agents["f/c/a"].restarts, 3);
        assert_eq!(
            s.entry("f/c/a").restarts,
            3,
            "second call returns the same entry"
        );
    }

    #[test]
    fn status_round_trips_and_tolerates_missing_optional_fields() {
        let s: FleetStatus = serde_json::from_value(json!({
            "generation": 2, "observed_generation": 1, "phase": "reconciling",
            "agents": { "f/c/a": { "phase": "starting" } }
        }))
        .unwrap();
        assert_eq!(s.agents["f/c/a"].applied_hash, None);
        let back: FleetStatus = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn activation_rows_are_optional_on_the_wire() {
        let s: AgentStatus = serde_json::from_value(json!({ "phase": "ready" })).unwrap();
        assert!(s.plugins.is_empty());
        let v = serde_json::to_value(&s).unwrap();
        assert!(v.get("plugins").is_none(), "empty map is skipped");
        let mut s = s;
        s.plugins
            .insert("flow".into(), PluginActivation::rejected("bad regex"));
        s.plugins.insert("web".into(), PluginActivation::active());
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["plugins"]["flow"]["state"], "rejected");
        assert_eq!(v["plugins"]["flow"]["message"], "bad regex");
        assert_eq!(v["plugins"]["web"]["state"], "active");
        assert_eq!(v["plugins"]["web"]["message"], "");
        let back: AgentStatus = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
        assert_eq!(
            PluginActivation::pending(),
            PluginActivation {
                state: ActivationState::Pending,
                message: String::new()
            }
        );
    }

    #[test]
    fn down_is_a_phase_and_summaries_round_trip() {
        assert_eq!(
            serde_json::to_value(FleetPhase::Down).unwrap(),
            json!("down")
        );
        let s = FleetSummary {
            name: "payments".into(),
            phase: FleetPhase::Ready,
            generation: 3,
            observed_generation: 3,
            agents: 2,
            managed_by: None,
        };
        let back: FleetSummary = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        let older: FleetSummary = serde_json::from_value(json!({
            "name": "p", "phase": "ready", "generation": 1, "observed_generation": 1, "agents": 0
        }))
        .unwrap();
        assert_eq!(older.managed_by, None);
        let v = serde_json::to_value(&older).unwrap();
        assert!(v.get("managed_by").is_none(), "omitted when unowned");
    }
}
