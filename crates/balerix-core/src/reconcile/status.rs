//! Folding step outcomes and external signals into `FleetStatus`
//! (Phase 2 spec §3.4). Pure.

use balerix_api::{AgentPhase, FleetPhase, FleetStatus, Timestamp};

use crate::name::AgentId;
use crate::reconcile::{ReconcilePolicy, Step, backoff_secs};

pub type StepOutcome = Result<(), String>;

pub fn apply(
    status: &mut FleetStatus,
    step: &Step,
    outcome: &StepOutcome,
    policy: &ReconcilePolicy,
    now: Timestamp,
) {
    match (step, outcome) {
        (Step::Stop(id), Ok(())) => {
            if let Some(a) = status.agents.get_mut(&id.to_string()) {
                a.phase = AgentPhase::Stopped;
                a.next_restart_at = None;
            }
        }
        (Step::RemoveAgent(id), Ok(())) => {
            status.agents.remove(&id.to_string());
        }
        (Step::RemoveCrew(crew, _), Ok(())) => {
            let prefix = format!("{crew}/");
            status.agents.retain(|k, _| !k.starts_with(&prefix));
        }
        (Step::RemoveCrew(crew, _), Err(e)) => {
            let prefix = format!("{crew}/");
            for (_, a) in status
                .agents
                .iter_mut()
                .filter(|(k, _)| k.starts_with(&prefix))
            {
                a.message.clone_from(e);
            }
        }
        (Step::EnsureCrew(_), _) => {}
        (Step::Materialize(id), Ok(())) => {
            let a = status.entry(&id.to_string());
            a.phase = AgentPhase::Materializing;
            a.message.clear();
        }
        (Step::Start(id, hash), Ok(())) => {
            let a = status.entry(&id.to_string());
            if a.applied_hash.as_ref() != Some(hash) {
                a.restarts = 0;
            }
            a.phase = AgentPhase::Starting;
            a.applied_hash = Some(hash.clone());
            a.next_restart_at = None;
            a.message.clear();
        }
        (Step::NoteExit(id, code), Ok(())) => {
            let a = status.entry(&id.to_string());
            a.restarts += 1;
            a.message = match code {
                Some(c) => format!("exited with status {c}"),
                None => "exited (killed by signal)".to_string(),
            };
            if a.restarts > policy.max_restarts {
                a.phase = AgentPhase::Dead;
                a.next_restart_at = None;
                a.message
                    .push_str(&format!("; gave up after {} restarts", policy.max_restarts));
            } else {
                a.next_restart_at = Some(now.plus_secs(backoff_secs(policy, a.restarts)));
            }
        }
        (Step::Materialize(id) | Step::Start(id, _), Err(e)) => {
            let a = status.entry(&id.to_string());
            a.phase = AgentPhase::Failed;
            a.message.clone_from(e);
        }
        (Step::Stop(id) | Step::RemoveAgent(id) | Step::NoteExit(id, _), Err(e)) => {
            status.entry(&id.to_string()).message.clone_from(e);
        }
    }
}

/// `SessionStart` reached the daemon: the agent is up and accepting input.
pub fn agent_ready(status: &mut FleetStatus, agent: &AgentId, now: Timestamp) {
    let Some(a) = status.agents.get_mut(&agent.to_string()) else {
        return;
    };
    a.phase = AgentPhase::Ready;
    a.restarts = 0;
    a.next_restart_at = None;
    a.last_event_at = Some(now);
    a.message.clear();
    let terminating = matches!(status.phase, FleetPhase::Terminating | FleetPhase::Down);
    status.phase = derive_fleet_phase(status, terminating, true);
}

pub fn set_desired(status: &mut FleetStatus, generation: u64) {
    status.generation = generation;
}

pub fn finish_pass(status: &mut FleetStatus, terminating: bool, all_ok: bool) {
    status.phase = derive_fleet_phase(status, terminating, all_ok);
    if all_ok {
        status.observed_generation = status.generation;
    }
}

fn derive_fleet_phase(status: &FleetStatus, terminating: bool, all_ok: bool) -> FleetPhase {
    if terminating {
        // Down only once a terminating pass ran clean and nothing is left
        // recorded; `agent_ready` (all_ok = true, agents non-empty) cannot
        // get here.
        return if all_ok && status.agents.is_empty() {
            FleetPhase::Down
        } else {
            FleetPhase::Terminating
        };
    }
    let phases = || status.agents.values().map(|a| a.phase);
    if !all_ok || phases().any(|p| matches!(p, AgentPhase::Dead | AgentPhase::Failed)) {
        FleetPhase::Degraded
    } else if phases().any(|p| {
        matches!(
            p,
            AgentPhase::Pending | AgentPhase::Materializing | AgentPhase::Starting
        )
    }) {
        FleetPhase::Reconciling
    } else if !status.agents.is_empty()
        && phases().all(|p| matches!(p, AgentPhase::Ready | AgentPhase::Stopped))
    {
        FleetPhase::Ready
    } else if status.agents.is_empty() {
        FleetPhase::Pending
    } else {
        FleetPhase::Reconciling
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::SpecHash;

    fn id(s: &str) -> AgentId {
        s.parse().unwrap()
    }
    fn h(s: &str) -> SpecHash {
        SpecHash::new(s.to_string())
    }
    fn policy() -> ReconcilePolicy {
        ReconcilePolicy {
            max_restarts: 2,
            backoff_base_secs: 2,
            backoff_cap_secs: 300,
        }
    }

    #[test]
    fn materialize_then_start_moves_through_phases_and_records_the_hash() {
        let mut s = FleetStatus::default();
        apply(
            &mut s,
            &Step::Materialize(id("f/c/a")),
            &Ok(()),
            &policy(),
            Timestamp(0),
        );
        assert_eq!(s.agents["f/c/a"].phase, AgentPhase::Materializing);
        apply(
            &mut s,
            &Step::Start(id("f/c/a"), h("aaaa")),
            &Ok(()),
            &policy(),
            Timestamp(0),
        );
        let a = &s.agents["f/c/a"];
        assert_eq!(a.phase, AgentPhase::Starting);
        assert_eq!(a.applied_hash, Some(h("aaaa")));
        assert_eq!(a.next_restart_at, None);
    }

    /// A failed `Materialize` or `Start` is visible in the phase, not only
    /// the message (#72); the next success clears both.
    #[test]
    fn a_failed_materialize_or_start_moves_the_agent_to_failed() {
        let mut s = FleetStatus::default();
        s.entry("f/c/a").phase = AgentPhase::Ready;
        apply(
            &mut s,
            &Step::Materialize(id("f/c/a")),
            &Err("f/c/a: git worktree: boom".into()),
            &policy(),
            Timestamp(0),
        );
        assert_eq!(s.agents["f/c/a"].phase, AgentPhase::Failed);
        assert_eq!(s.agents["f/c/a"].message, "f/c/a: git worktree: boom");
        apply(
            &mut s,
            &Step::Materialize(id("f/c/a")),
            &Ok(()),
            &policy(),
            Timestamp(0),
        );
        assert_eq!(s.agents["f/c/a"].phase, AgentPhase::Materializing);
        assert_eq!(s.agents["f/c/a"].message, "", "success clears the message");
        apply(
            &mut s,
            &Step::Start(id("f/c/a"), SpecHash::new("h".into())),
            &Err("f/c/a: tmux: no server".into()),
            &policy(),
            Timestamp(0),
        );
        assert_eq!(s.agents["f/c/a"].phase, AgentPhase::Failed);
    }

    /// Other failed steps only set the message: a failed `Stop` leaves the
    /// session running, so the phase still describes it.
    #[test]
    fn a_failed_stop_sets_the_message_and_keeps_the_phase() {
        let mut s = FleetStatus::default();
        s.entry("f/c/a").phase = AgentPhase::Ready;
        apply(
            &mut s,
            &Step::Stop(id("f/c/a")),
            &Err("f/c/a: tmux kill-window: boom".into()),
            &policy(),
            Timestamp(0),
        );
        assert_eq!(s.agents["f/c/a"].phase, AgentPhase::Ready);
        assert_eq!(s.agents["f/c/a"].message, "f/c/a: tmux kill-window: boom");
    }

    /// A failed agent degrades the fleet even on a pass that ran clean for
    /// everyone else (a sibling's `SessionStart`, say).
    #[test]
    fn a_failed_agent_degrades_the_fleet() {
        let mut s = FleetStatus::default();
        s.entry("f/c/a").phase = AgentPhase::Failed;
        s.entry("f/c/b").phase = AgentPhase::Starting;
        agent_ready(&mut s, &id("f/c/b"), Timestamp(5));
        assert_eq!(s.agents["f/c/b"].phase, AgentPhase::Ready);
        assert_eq!(s.phase, FleetPhase::Degraded);
    }

    #[test]
    fn exits_back_off_then_give_up() {
        let mut s = FleetStatus::default();
        apply(
            &mut s,
            &Step::Start(id("f/c/a"), h("x")),
            &Ok(()),
            &policy(),
            Timestamp(0),
        );
        apply(
            &mut s,
            &Step::NoteExit(id("f/c/a"), Some(1)),
            &Ok(()),
            &policy(),
            Timestamp(100),
        );
        let a = &s.agents["f/c/a"];
        assert_eq!(
            (a.restarts, a.next_restart_at, a.phase),
            (1, Some(Timestamp(102)), AgentPhase::Starting)
        );
        assert_eq!(a.message, "exited with status 1");
        apply(
            &mut s,
            &Step::Start(id("f/c/a"), h("x")),
            &Ok(()),
            &policy(),
            Timestamp(102),
        );
        assert_eq!(s.agents["f/c/a"].restarts, 1, "same hash keeps the count");
        apply(
            &mut s,
            &Step::NoteExit(id("f/c/a"), None),
            &Ok(()),
            &policy(),
            Timestamp(200),
        );
        assert_eq!(s.agents["f/c/a"].next_restart_at, Some(Timestamp(204)));
        apply(
            &mut s,
            &Step::Start(id("f/c/a"), h("x")),
            &Ok(()),
            &policy(),
            Timestamp(204),
        );
        apply(
            &mut s,
            &Step::NoteExit(id("f/c/a"), Some(2)),
            &Ok(()),
            &policy(),
            Timestamp(300),
        );
        let a = &s.agents["f/c/a"];
        assert_eq!(a.phase, AgentPhase::Dead);
        assert_eq!(a.next_restart_at, None);
        assert_eq!(a.message, "exited with status 2; gave up after 2 restarts");
    }

    #[test]
    fn a_new_hash_resets_the_restart_count() {
        let mut s = FleetStatus::default();
        apply(
            &mut s,
            &Step::Start(id("f/c/a"), h("x")),
            &Ok(()),
            &policy(),
            Timestamp(0),
        );
        apply(
            &mut s,
            &Step::NoteExit(id("f/c/a"), Some(1)),
            &Ok(()),
            &policy(),
            Timestamp(1),
        );
        apply(
            &mut s,
            &Step::Start(id("f/c/a"), h("y")),
            &Ok(()),
            &policy(),
            Timestamp(3),
        );
        assert_eq!(s.agents["f/c/a"].restarts, 0);
    }

    #[test]
    fn ready_signal_resets_restarts_and_stamps_the_event() {
        let mut s = FleetStatus::default();
        apply(
            &mut s,
            &Step::Start(id("f/c/a"), h("x")),
            &Ok(()),
            &policy(),
            Timestamp(0),
        );
        s.agents.get_mut("f/c/a").unwrap().restarts = 2;
        agent_ready(&mut s, &id("f/c/a"), Timestamp(7));
        let a = &s.agents["f/c/a"];
        assert_eq!(
            (a.phase, a.restarts, a.last_event_at),
            (AgentPhase::Ready, 0, Some(Timestamp(7)))
        );
        assert_eq!(s.phase, FleetPhase::Ready);
        agent_ready(&mut s, &id("f/c/unknown"), Timestamp(8));
        assert!(!s.agents.contains_key("f/c/unknown"));
    }

    #[test]
    fn removals_drop_entries() {
        let mut s = FleetStatus::default();
        s.entry("f/c/a");
        s.entry("f/c/b");
        s.entry("f/d/x");
        apply(
            &mut s,
            &Step::Stop(id("f/c/a")),
            &Ok(()),
            &policy(),
            Timestamp(0),
        );
        assert_eq!(s.agents["f/c/a"].phase, AgentPhase::Stopped);
        apply(
            &mut s,
            &Step::RemoveAgent(id("f/c/a")),
            &Ok(()),
            &policy(),
            Timestamp(0),
        );
        assert!(!s.agents.contains_key("f/c/a"));
        apply(
            &mut s,
            &Step::RemoveCrew("f/c".parse().unwrap(), Default::default()),
            &Ok(()),
            &policy(),
            Timestamp(0),
        );
        assert_eq!(s.agents.keys().collect::<Vec<_>>(), vec!["f/d/x"]);
    }

    #[test]
    fn a_stop_clears_the_pending_restart_and_counts_as_settled() {
        let mut s = FleetStatus::default();
        apply(
            &mut s,
            &Step::Start(id("f/c/a"), h("x")),
            &Ok(()),
            &policy(),
            Timestamp(0),
        );
        apply(
            &mut s,
            &Step::NoteExit(id("f/c/a"), Some(1)),
            &Ok(()),
            &policy(),
            Timestamp(10),
        );
        assert!(s.agents["f/c/a"].next_restart_at.is_some());
        apply(
            &mut s,
            &Step::Stop(id("f/c/a")),
            &Ok(()),
            &policy(),
            Timestamp(11),
        );
        let a = &s.agents["f/c/a"];
        assert_eq!(
            (a.phase, a.next_restart_at, a.restarts),
            (AgentPhase::Stopped, None, 1)
        );
        s.entry("f/c/b").phase = AgentPhase::Ready;
        finish_pass(&mut s, false, true);
        assert_eq!(
            s.phase,
            FleetPhase::Ready,
            "stopped agents do not hold the fleet"
        );
        s.entry("f/c/b").phase = AgentPhase::Starting;
        finish_pass(&mut s, false, true);
        assert_eq!(s.phase, FleetPhase::Reconciling);
    }

    #[test]
    fn fleet_phase_is_derived_and_observed_generation_follows_success() {
        let mut s = FleetStatus::default();
        set_desired(&mut s, 3);
        finish_pass(&mut s, false, true);
        assert_eq!((s.phase, s.observed_generation), (FleetPhase::Pending, 3));
        s.entry("f/c/a").phase = AgentPhase::Starting;
        set_desired(&mut s, 4);
        finish_pass(&mut s, false, false);
        assert_eq!((s.phase, s.observed_generation), (FleetPhase::Degraded, 3));
        finish_pass(&mut s, false, true);
        assert_eq!(
            (s.phase, s.observed_generation),
            (FleetPhase::Reconciling, 4)
        );
        s.entry("f/c/a").phase = AgentPhase::Ready;
        finish_pass(&mut s, false, true);
        assert_eq!(s.phase, FleetPhase::Ready);
        s.entry("f/c/b").phase = AgentPhase::Dead;
        finish_pass(&mut s, false, true);
        assert_eq!(s.phase, FleetPhase::Degraded);
        finish_pass(&mut s, true, true);
        assert_eq!(
            s.phase,
            FleetPhase::Terminating,
            "agents still recorded: not done"
        );
        s.agents.clear();
        finish_pass(&mut s, true, false);
        assert_eq!(s.phase, FleetPhase::Terminating, "a failed step keeps it");
        finish_pass(&mut s, true, true);
        assert_eq!(s.phase, FleetPhase::Down);
    }
}
