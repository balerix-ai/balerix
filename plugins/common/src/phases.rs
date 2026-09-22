//! Agent phase changes from `fleets/watch` (Spec G §8's phase feed). The
//! watch yields whole snapshots, so a *change* is derived by diffing the
//! new frame against the previous one, keyed by each agent's full id.

use std::collections::BTreeMap;
use std::convert::Infallible;

use balerix_api::{AgentPhase, AgentStatus, FleetRecord};
use balerix_plugin_sdk::Host;

/// One agent's phase transition, from the fleet watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseChange {
    /// The agent that changed phase.
    pub agent: String,
    /// The phase it left.
    pub from: AgentPhase,
    /// The phase it entered.
    pub to: AgentPhase,
    /// Why, for the thread.
    pub message: String,
}

/// The phase changes between two `fleets/watch` frames (Spec G §8's phase
/// feed). The watch yields a full snapshot each time, not a delta
/// (`FleetWatch::next` docs), so a phase *change* has to be derived here,
/// by diffing the new snapshot against the previous one, keyed by each
/// agent's full id (`fleet/crew/agent`, as `FleetStatus::agents` keys it).
///
/// `previous` is `None` on the very first frame: that frame is the
/// fleet's current state, not a change, and diffing it against nothing
/// would post a phase message for every agent in the fleet at every
/// plugin start. So the first frame yields no changes — the caller is
/// expected to keep it only as next call's `previous`.
///
/// An agent that just appeared has no prior phase to report a change
/// from, and one that disappeared has no new phase to report a change
/// to; neither yields a row — only an agent present in both snapshots
/// with a phase that differs does.
pub fn phase_changes(
    previous: Option<&[FleetRecord]>,
    current: &[FleetRecord],
) -> Vec<PhaseChange> {
    let Some(previous) = previous else {
        return Vec::new();
    };
    let before = flatten(previous);
    flatten(current)
        .into_iter()
        .filter_map(|(agent, status)| {
            let prior = before.get(&agent)?;
            if prior.phase == status.phase {
                return None;
            }
            Some(PhaseChange {
                agent,
                from: prior.phase,
                to: status.phase,
                message: status.message,
            })
        })
        .collect()
}

/// Every agent across every fleet in one `fleets/watch` frame, keyed by
/// its full id.
fn flatten(fleets: &[FleetRecord]) -> BTreeMap<String, AgentStatus> {
    fleets
        .iter()
        .flat_map(|f| {
            f.status
                .agents
                .iter()
                .map(|(id, s)| (id.clone(), s.clone()))
        })
        .collect()
}

/// The `fleets/watch` loop: keeps the previous frame, hands every
/// non-empty diff to `sink`, and never returns — `FleetWatch::next`
/// reconnects on its own. Spawn it and abort the task at exit.
pub async fn run(host: Host, mut sink: impl FnMut(Vec<PhaseChange>) + Send) -> Infallible {
    let mut watch = host.watch_fleets();
    let mut previous: Option<Vec<FleetRecord>> = None;
    loop {
        let current = watch.next().await;
        let changes = phase_changes(previous.as_deref(), &current);
        if !changes.is_empty() {
            sink(changes);
        }
        previous = Some(current);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::FleetSpec;
    use balerix_plugin_sdk::testing::FakeHost;
    use std::sync::{Arc, Mutex};

    /// One `fleets/watch` frame: a single fleet carrying exactly the
    /// agents given, each at its phase and status message.
    fn fleet(agents: &[(&str, AgentPhase, &str)]) -> Vec<FleetRecord> {
        let mut record = FleetRecord::new(FleetSpec {
            name: "f".into(),
            ..Default::default()
        });
        for (id, phase, message) in agents {
            record.status.agents.insert(
                (*id).to_string(),
                AgentStatus {
                    phase: *phase,
                    message: (*message).to_string(),
                    ..Default::default()
                },
            );
        }
        vec![record]
    }

    #[test]
    fn the_first_frame_seeds_silently_and_reports_nothing() {
        let current = fleet(&[("f/c/alice", AgentPhase::Ready, "")]);
        assert_eq!(phase_changes(None, &current), Vec::new());
    }

    #[test]
    fn an_agent_that_changed_phase_is_reported() {
        let previous = fleet(&[("f/c/alice", AgentPhase::Starting, "")]);
        let current = fleet(&[("f/c/alice", AgentPhase::Ready, "agent ready")]);
        assert_eq!(
            phase_changes(Some(&previous), &current),
            vec![PhaseChange {
                agent: "f/c/alice".into(),
                from: AgentPhase::Starting,
                to: AgentPhase::Ready,
                message: "agent ready".into(),
            }]
        );
    }

    #[test]
    fn an_agent_whose_phase_held_is_not_reported() {
        let previous = fleet(&[("f/c/alice", AgentPhase::Ready, "")]);
        let current = fleet(&[("f/c/alice", AgentPhase::Ready, "")]);
        assert_eq!(phase_changes(Some(&previous), &current), Vec::new());
    }

    #[test]
    fn an_agent_that_appeared_or_disappeared_is_not_reported() {
        let none = fleet(&[]);
        let one = fleet(&[("f/c/alice", AgentPhase::Pending, "")]);
        assert_eq!(phase_changes(Some(&none), &one), Vec::new());
        assert_eq!(phase_changes(Some(&one), &none), Vec::new());
    }

    #[tokio::test]
    async fn run_feeds_the_sink_with_changes_between_frames() {
        let fake = FakeHost::start(
            "tok",
            serde_json::json!({}),
            fleet(&[("f/c/a", AgentPhase::Starting, "")]),
        )
        .await;
        let host = Host::new(fake.env("t", std::path::Path::new("scratch"))).unwrap();
        let seen: Arc<Mutex<Vec<PhaseChange>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let seen = seen.clone();
            move |changes: Vec<PhaseChange>| seen.lock().unwrap().extend(changes)
        };
        let task = tokio::spawn(run(host, sink));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        fake.set_fleets(fleet(&[("f/c/a", AgentPhase::Ready, "up")]));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while seen.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        task.abort();
        let got = seen.lock().unwrap().clone();
        assert_eq!(
            got.len(),
            1,
            "the first frame seeds; the second reports: {got:?}"
        );
        assert_eq!(got[0].to, AgentPhase::Ready);
    }
}
