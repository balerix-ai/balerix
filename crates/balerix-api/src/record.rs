//! One fleet as the daemon stores and returns it (Phase 3 spec §2). A wire
//! type: the CLI, the plugin SDK and the daemon all read it.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{FleetPhase, FleetSpec, FleetStatus, FleetSummary};

/// What `down` leaves behind (spec D6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Keep {
    pub repos: bool,
    pub sessions: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetRecord {
    pub spec: FleetSpec,
    pub generation: u64,
    pub desired: Desired,
    /// Agents held stopped by a plugin action (plugins spec §16.4): stopped
    /// if observed, never restarted, restart counter untouched. Keyed like
    /// `status.agents`. Cleared for every agent an `Apply` declares.
    #[serde(default)]
    pub stopped: BTreeSet<String>,
    pub status: FleetStatus,
}

/// Whether the fleet should be running (P3-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Desired {
    Up,
    Down { keep: Keep, purge: bool },
}

impl FleetRecord {
    pub fn new(spec: FleetSpec) -> Self {
        Self {
            spec,
            generation: 0,
            desired: Desired::Up,
            stopped: BTreeSet::new(),
            status: FleetStatus::default(),
        }
    }
    pub fn name(&self) -> &str {
        &self.spec.name
    }
    /// Downed and settled: `up` may re-apply in place, `POST` is not a 409.
    pub fn is_down(&self) -> bool {
        matches!(self.desired, Desired::Down { .. }) && self.status.phase == FleetPhase::Down
    }
    pub fn summary(&self) -> FleetSummary {
        FleetSummary {
            name: self.spec.name.clone(),
            phase: self.status.phase,
            generation: self.generation,
            observed_generation: self.status.observed_generation,
            agents: self.status.agents.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FleetPhase;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn spec() -> FleetSpec {
        FleetSpec {
            name: "payments".into(),
            crews: BTreeMap::new(),
            ..Default::default()
        }
    }

    #[test]
    fn a_new_record_is_up_at_generation_zero() {
        let r = FleetRecord::new(spec());
        assert_eq!(r.generation, 0);
        assert_eq!(r.desired, Desired::Up);
        assert_eq!(r.status.phase, FleetPhase::Pending);
        assert_eq!(r.name(), "payments");
        assert!(!r.is_down());
        let s = r.summary();
        assert_eq!(
            (s.name.as_str(), s.generation, s.agents),
            ("payments", 0, 0)
        );
    }

    #[test]
    fn desired_serializes_with_a_state_tag() {
        assert_eq!(
            serde_json::to_value(Desired::Up).unwrap(),
            json!({ "state": "up" })
        );
        let d = Desired::Down {
            keep: Keep {
                repos: true,
                sessions: false,
            },
            purge: false,
        };
        let v = serde_json::to_value(d).unwrap();
        assert_eq!(v["state"], "down");
        assert_eq!(v["keep"]["repos"], true);
        let back: Desired = serde_json::from_value(v).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn is_down_needs_both_the_desire_and_the_phase() {
        let mut r = FleetRecord::new(spec());
        r.desired = Desired::Down {
            keep: Keep::default(),
            purge: false,
        };
        assert!(!r.is_down(), "still terminating");
        r.status.phase = FleetPhase::Down;
        assert!(r.is_down());
    }

    #[test]
    fn stopped_defaults_empty_and_round_trips() {
        let mut r = FleetRecord::new(spec());
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["stopped"], serde_json::json!([]));
        let older: FleetRecord = serde_json::from_value(serde_json::json!({
            "spec": { "name": "payments" }, "generation": 1, "desired": { "state": "up" },
            "status": { "generation": 1, "observed_generation": 1, "phase": "ready" }
        }))
        .unwrap();
        assert!(older.stopped.is_empty(), "a phase 1 fleet.json loads");
        r.stopped.insert("payments/backend/bob".into());
        let back: FleetRecord = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back.stopped, r.stopped);
    }
}
