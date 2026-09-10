//! The reconciler (Phase 2 spec §3): `plan` decides, `execute` acts,
//! `apply` folds each outcome into the status. Nothing here does I/O.

mod execute;
mod status;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use balerix_api::{AgentPhase, FleetStatus, SpecHash, Timestamp};

use crate::agent::{CrewRef, ResolvedAgent};
use crate::fleet::Fleet;
use crate::name::{AgentId, FleetName};
use crate::ports::{Keep, ObservedState, ProcessState};

pub use execute::{ExecuteReport, ReconcileContext, execute, reconcile_pass};
pub use status::{StepOutcome, agent_ready, apply, finish_pass, set_desired};

/// Restart limits (spec §7 "bounded exponential backoff").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcilePolicy {
    pub max_restarts: u32,
    pub backoff_base_secs: u64,
    pub backoff_cap_secs: u64,
}

impl Default for ReconcilePolicy {
    fn default() -> Self {
        Self {
            max_restarts: 5,
            backoff_base_secs: 2,
            backoff_cap_secs: 300,
        }
    }
}

/// One thing the executor does. Steps carry ids, never data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Stop(AgentId),
    RemoveAgent(AgentId),
    RemoveCrew(CrewRef, Keep),
    EnsureCrew(CrewRef),
    Materialize(AgentId),
    Start(AgentId, SpecHash),
    NoteExit(AgentId, Option<i32>),
}

impl Step {
    pub fn agent_id(&self) -> Option<&AgentId> {
        match self {
            Step::Stop(id)
            | Step::RemoveAgent(id)
            | Step::Materialize(id)
            | Step::Start(id, _)
            | Step::NoteExit(id, _) => Some(id),
            Step::RemoveCrew(..) | Step::EnsureCrew(_) => None,
        }
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Stop(id) => write!(f, "stop {id}"),
            Step::RemoveAgent(id) => write!(f, "remove-agent {id}"),
            Step::RemoveCrew(c, k) => write!(
                f,
                "remove-crew {c} keep-repos={} keep-sessions={}",
                k.repos, k.sessions
            ),
            Step::EnsureCrew(c) => write!(f, "ensure-crew {c}"),
            Step::Materialize(id) => write!(f, "materialize {id}"),
            Step::Start(id, h) => {
                write!(f, "start {id} {}", &h.as_str()[..8.min(h.as_str().len())])
            }
            Step::NoteExit(id, code) => write!(f, "note-exit {id} {code:?}"),
        }
    }
}

pub type Plan = Vec<Step>;

/// Seconds to wait before restart number `restarts` (1-based).
pub fn backoff_secs(policy: &ReconcilePolicy, restarts: u32) -> u64 {
    if restarts == 0 {
        return 0;
    }
    let factor = 1u64.checked_shl(restarts - 1).unwrap_or(u64::MAX);
    policy
        .backoff_base_secs
        .saturating_mul(factor)
        .min(policy.backoff_cap_secs)
}

/// Decides what to do. Pure. Ordering: stops, agent removals, crew removals,
/// crew ensures, then per agent (sorted by id) either `Materialize`+`Start`
/// or `NoteExit`.
///
/// Per desired agent, with `changed = status.applied_hash != desired hash`:
///
/// | observed | condition | steps |
/// |---|---|---|
/// | any | in `stopped` | `Stop` if observed, else — |
/// | `Running` | `!changed` | — |
/// | `Running` | `changed` | `Stop`, `Materialize`, `Start` |
/// | `Exited` | `changed` | `Materialize`, `Start` |
/// | `Exited` | phase `Dead` | — |
/// | `Exited` | `next_restart_at.is_none()` (exit not yet noted) | `NoteExit` |
/// | `Exited` | `next_restart_at <= now` | `Materialize`, `Start` |
/// | `Exited` | otherwise (backoff running) | — |
/// | absent | phase `Dead` and `!changed` | — |
/// | absent | otherwise | `Materialize`, `Start` |
///
/// A desired agent in `stopped` (plugins spec §16.4) gets `Stop` if it is
/// observed at all and nothing else: no restart, no `NoteExit`, even when
/// its hash changed. Leaving the set is an ordinary "absent → restart".
///
/// Known-but-not-desired agents get `Stop` (if observed) and `RemoveAgent`;
/// their crews, if no longer desired, `RemoveCrew` with the caller's `keep`.
/// With `desired == None` (down) every observed agent is stopped and every
/// known crew removed with `keep`; nothing is ensured.
#[allow(clippy::too_many_arguments)]
pub fn plan(
    fleet: &FleetName,
    desired: Option<&Fleet>,
    keep: Keep,
    stopped: &BTreeSet<AgentId>,
    status: &FleetStatus,
    observed: &ObservedState,
    _policy: &ReconcilePolicy,
    now: Timestamp,
) -> Plan {
    let desired_agents: BTreeMap<AgentId, ResolvedAgent> = desired
        .map(|f| {
            ResolvedAgent::from_fleet(f)
                .into_iter()
                .map(|a| (a.id.clone(), a))
                .collect()
        })
        .unwrap_or_default();
    let desired_crews: BTreeSet<CrewRef> = desired_agents.keys().map(AgentId::crew_ref).collect();

    let mut known_agents: BTreeSet<AgentId> = status
        .agents
        .keys()
        .filter_map(|k| k.parse().ok())
        .collect();
    known_agents.extend(observed.agent_ids(fleet));
    let mut known_crews: BTreeSet<CrewRef> = known_agents.iter().map(AgentId::crew_ref).collect();
    known_crews.extend(observed.crews.keys().map(|c| CrewRef {
        fleet: fleet.clone(),
        crew: c.clone(),
    }));

    let mut stops = Vec::new();
    let mut remove_agents = Vec::new();
    let mut remove_crews = Vec::new();
    let mut ensures = Vec::new();
    let mut agent_steps = Vec::new();

    for id in &known_agents {
        if desired_agents.contains_key(id) {
            continue;
        }
        if observed.get(id).is_some() {
            stops.push(Step::Stop(id.clone()));
        }
        if desired.is_some() {
            remove_agents.push(Step::RemoveAgent(id.clone()));
        }
    }
    for crew in &known_crews {
        if !desired_crews.contains(crew) {
            remove_crews.push(Step::RemoveCrew(crew.clone(), keep));
        }
    }
    for crew in &desired_crews {
        ensures.push(Step::EnsureCrew(crew.clone()));
    }

    for (id, agent) in &desired_agents {
        if stopped.contains(id) {
            if observed.get(id).is_some() {
                stops.push(Step::Stop(id.clone()));
            }
            continue;
        }
        let hash = agent.hash();
        let st = status.agents.get(&id.to_string());
        let changed = st.and_then(|s| s.applied_hash.as_ref()) != Some(&hash);
        let dead = st.is_some_and(|s| s.phase == AgentPhase::Dead);
        let restart = |steps: &mut Vec<Step>| {
            steps.push(Step::Materialize(id.clone()));
            steps.push(Step::Start(id.clone(), hash.clone()));
        };
        match observed.get(id) {
            Some(ProcessState::Running { .. }) if !changed => {}
            Some(ProcessState::Running { .. }) => {
                stops.push(Step::Stop(id.clone()));
                restart(&mut agent_steps);
            }
            Some(ProcessState::Exited { code }) => {
                if changed {
                    restart(&mut agent_steps);
                } else if dead {
                    // gave up already; leave it alone until the hash changes
                } else {
                    match st.and_then(|s| s.next_restart_at) {
                        None => agent_steps.push(Step::NoteExit(id.clone(), *code)),
                        Some(due) if due <= now => restart(&mut agent_steps),
                        Some(_) => {}
                    }
                }
            }
            None => {
                if !(dead && !changed) {
                    restart(&mut agent_steps);
                }
            }
        }
    }

    stops.sort_by_key(|s| s.agent_id().cloned());

    let mut out = stops;
    out.extend(remove_agents);
    out.extend(remove_crews);
    out.extend(ensures);
    out.extend(agent_steps);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{AgentSettings, AgentStatus, CrewSpec, FleetSpec, GitSettings};
    use std::collections::BTreeMap;

    fn fleet_name() -> FleetName {
        "f".parse().unwrap()
    }

    fn id(s: &str) -> AgentId {
        s.parse().unwrap()
    }

    /// A fleet "f" with crew "c" and the given agents; `env.V` carries a
    /// per-agent version so the hash can be changed on purpose.
    fn fleet(agents: &[(&str, u32)]) -> Fleet {
        Fleet::try_from(FleetSpec {
            name: "f".into(),
            crews: BTreeMap::from([(
                "c".to_string(),
                CrewSpec {
                    repo: "acme/api".into(),
                    git_ref: "main".into(),
                    git: GitSettings::default(),
                    agents: agents
                        .iter()
                        .map(|(n, v)| {
                            let mut s = AgentSettings::default();
                            s.env.insert("V".into(), v.to_string());
                            (n.to_string(), s)
                        })
                        .collect(),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        })
        .unwrap()
    }

    fn hash_of(f: &Fleet, agent: &str) -> SpecHash {
        ResolvedAgent::from_fleet(f)
            .into_iter()
            .find(|a| a.id.agent.as_str() == agent)
            .unwrap()
            .hash()
    }

    fn render(p: &Plan) -> Vec<String> {
        p.iter().map(ToString::to_string).collect()
    }

    fn short(h: &SpecHash) -> String {
        h.as_str()[..8].to_string()
    }

    fn plan_for(
        desired: Option<&Fleet>,
        stopped: &[&str],
        status: &FleetStatus,
        observed: &ObservedState,
        now: u64,
    ) -> Vec<String> {
        let stopped: BTreeSet<AgentId> = stopped.iter().map(|s| id(s)).collect();
        render(&plan(
            &fleet_name(),
            desired,
            Keep::default(),
            &stopped,
            status,
            observed,
            &ReconcilePolicy::default(),
            Timestamp(now),
        ))
    }

    #[test]
    fn backoff_doubles_from_base_and_caps() {
        let p = ReconcilePolicy::default();
        assert_eq!(backoff_secs(&p, 0), 0);
        assert_eq!(backoff_secs(&p, 1), 2);
        assert_eq!(backoff_secs(&p, 2), 4);
        assert_eq!(backoff_secs(&p, 8), 256);
        assert_eq!(backoff_secs(&p, 9), 300);
        assert_eq!(backoff_secs(&p, 200), 300, "no shift overflow");
    }

    #[test]
    fn fresh_fleet_ensures_crew_then_materializes_and_starts_each_agent() {
        let f = fleet(&[("b", 1), ("a", 1)]);
        let got = plan_for(
            Some(&f),
            &[],
            &FleetStatus::default(),
            &ObservedState::default(),
            0,
        );
        assert_eq!(
            got,
            vec![
                "ensure-crew f/c".to_string(),
                "materialize f/c/a".to_string(),
                format!("start f/c/a {}", short(&hash_of(&f, "a"))),
                "materialize f/c/b".to_string(),
                format!("start f/c/b {}", short(&hash_of(&f, "b"))),
            ]
        );
    }

    #[test]
    fn ready_and_unchanged_fleet_plans_only_ensure_crew() {
        let f = fleet(&[("a", 1)]);
        let mut st = FleetStatus::default();
        st.entry("f/c/a").applied_hash = Some(hash_of(&f, "a"));
        st.entry("f/c/a").phase = AgentPhase::Ready;
        let mut obs = ObservedState::default();
        obs.set(&id("f/c/a"), ProcessState::Running { pid: 1 });
        assert_eq!(
            plan_for(Some(&f), &[], &st, &obs, 0),
            vec!["ensure-crew f/c"]
        );
    }

    #[test]
    fn changed_hash_on_a_running_agent_stops_then_restarts_it() {
        let old = fleet(&[("a", 1)]);
        let new = fleet(&[("a", 2)]);
        let mut st = FleetStatus::default();
        st.entry("f/c/a").applied_hash = Some(hash_of(&old, "a"));
        let mut obs = ObservedState::default();
        obs.set(&id("f/c/a"), ProcessState::Running { pid: 1 });
        assert_eq!(
            plan_for(Some(&new), &[], &st, &obs, 0),
            vec![
                "stop f/c/a".to_string(),
                "ensure-crew f/c".into(),
                "materialize f/c/a".into(),
                format!("start f/c/a {}", short(&hash_of(&new, "a")))
            ]
        );
    }

    #[test]
    fn undesired_agents_and_crews_are_stopped_and_removed() {
        let f = fleet(&[("a", 1)]);
        let mut st = FleetStatus::default();
        st.entry("f/old/z").phase = AgentPhase::Ready;
        let mut obs = ObservedState::default();
        obs.set(&id("f/c/gone"), ProcessState::Running { pid: 1 });
        let got = plan_for(Some(&f), &[], &st, &obs, 0);
        assert_eq!(got[0], "stop f/c/gone");
        assert_eq!(got[1], "remove-agent f/c/gone");
        assert_eq!(got[2], "remove-agent f/old/z");
        assert_eq!(
            got[3],
            "remove-crew f/old keep-repos=false keep-sessions=false"
        );
        assert_eq!(got[4], "ensure-crew f/c");
        assert!(got[5].starts_with("materialize f/c/a"));
    }

    #[test]
    fn an_undesired_crew_is_removed_with_the_callers_keep() {
        let f = fleet(&[("a", 1)]);
        let mut st = FleetStatus::default();
        st.entry("f/old/z").phase = AgentPhase::Ready;
        let got = render(&plan(
            &fleet_name(),
            Some(&f),
            Keep {
                repos: true,
                sessions: false,
            },
            &BTreeSet::new(),
            &st,
            &ObservedState::default(),
            &ReconcilePolicy::default(),
            Timestamp(0),
        ));
        assert!(
            got.contains(&"remove-crew f/old keep-repos=true keep-sessions=false".to_string()),
            "{got:?}"
        );
    }

    #[test]
    fn down_stops_everything_and_removes_crews_with_keep() {
        let mut st = FleetStatus::default();
        st.entry("f/c/a").phase = AgentPhase::Ready;
        let mut obs = ObservedState::default();
        obs.set(&id("f/c/a"), ProcessState::Running { pid: 1 });
        obs.set(&id("f/d/b"), ProcessState::Exited { code: Some(0) });
        let got = render(&plan(
            &fleet_name(),
            None,
            Keep {
                repos: true,
                sessions: false,
            },
            &BTreeSet::new(),
            &st,
            &obs,
            &ReconcilePolicy::default(),
            Timestamp(0),
        ));
        assert_eq!(
            got,
            vec![
                "stop f/c/a",
                "stop f/d/b",
                "remove-crew f/c keep-repos=true keep-sessions=false",
                "remove-crew f/d keep-repos=true keep-sessions=false",
            ]
        );
    }

    #[test]
    fn an_unnoted_exit_is_noted_not_restarted() {
        let f = fleet(&[("a", 1)]);
        let mut st = FleetStatus::default();
        st.entry("f/c/a").applied_hash = Some(hash_of(&f, "a"));
        st.entry("f/c/a").phase = AgentPhase::Ready;
        let mut obs = ObservedState::default();
        obs.set(&id("f/c/a"), ProcessState::Exited { code: Some(137) });
        assert_eq!(
            plan_for(Some(&f), &[], &st, &obs, 0),
            vec!["ensure-crew f/c", "note-exit f/c/a Some(137)"]
        );
    }

    #[test]
    fn a_noted_exit_restarts_only_once_due() {
        let f = fleet(&[("a", 1)]);
        let mut st = FleetStatus::default();
        *st.entry("f/c/a") = AgentStatus {
            phase: AgentPhase::Ready,
            applied_hash: Some(hash_of(&f, "a")),
            restarts: 1,
            next_restart_at: Some(Timestamp(10)),
            ..AgentStatus::default()
        };
        let mut obs = ObservedState::default();
        obs.set(&id("f/c/a"), ProcessState::Exited { code: None });
        assert_eq!(
            plan_for(Some(&f), &[], &st, &obs, 9),
            vec!["ensure-crew f/c"]
        );
        let due = plan_for(Some(&f), &[], &st, &obs, 10);
        assert_eq!(due.len(), 3);
        assert_eq!(due[1], "materialize f/c/a");
    }

    #[test]
    fn dead_agents_are_left_alone_until_the_hash_changes() {
        let f = fleet(&[("a", 1)]);
        let mut st = FleetStatus::default();
        *st.entry("f/c/a") = AgentStatus {
            phase: AgentPhase::Dead,
            applied_hash: Some(hash_of(&f, "a")),
            restarts: 6,
            ..AgentStatus::default()
        };
        let mut obs = ObservedState::default();
        obs.set(&id("f/c/a"), ProcessState::Exited { code: Some(1) });
        assert_eq!(
            plan_for(Some(&f), &[], &st, &obs, 0),
            vec!["ensure-crew f/c"]
        );
        // window vanished entirely: still left alone
        assert_eq!(
            plan_for(Some(&f), &[], &st, &ObservedState::default(), 0),
            vec!["ensure-crew f/c"]
        );
        // new spec: restart
        let f2 = fleet(&[("a", 2)]);
        assert_eq!(plan_for(Some(&f2), &[], &st, &obs, 0).len(), 3);
    }

    #[test]
    fn a_vanished_window_is_recreated() {
        let f = fleet(&[("a", 1)]);
        let mut st = FleetStatus::default();
        *st.entry("f/c/a") = AgentStatus {
            phase: AgentPhase::Ready,
            applied_hash: Some(hash_of(&f, "a")),
            ..AgentStatus::default()
        };
        let got = plan_for(Some(&f), &[], &st, &ObservedState::default(), 0);
        assert_eq!(got.len(), 3);
        assert_eq!(got[1], "materialize f/c/a");
    }

    #[test]
    fn a_stopped_agent_is_stopped_and_never_restarted() {
        let f = fleet(&[("a", 1), ("b", 1)]);
        let mut st = FleetStatus::default();
        for a in ["a", "b"] {
            let e = st.entry(&format!("f/c/{a}"));
            e.applied_hash = Some(hash_of(&f, a));
            e.phase = AgentPhase::Ready;
        }
        let mut obs = ObservedState::default();
        obs.set(&id("f/c/a"), ProcessState::Running { pid: 1 });
        obs.set(&id("f/c/b"), ProcessState::Running { pid: 2 });
        assert_eq!(
            plan_for(Some(&f), &["f/c/b"], &st, &obs, 0),
            vec!["stop f/c/b", "ensure-crew f/c"],
            "running: stopped, a untouched"
        );
        // exited with a due restart: still nothing but the stop
        st.entry("f/c/b").next_restart_at = Some(Timestamp(0));
        obs.set(&id("f/c/b"), ProcessState::Exited { code: Some(1) });
        assert_eq!(
            plan_for(Some(&f), &["f/c/b"], &st, &obs, 5),
            vec!["stop f/c/b", "ensure-crew f/c"]
        );
        // window gone, phase Stopped: nothing at all
        st.entry("f/c/b").phase = AgentPhase::Stopped;
        st.entry("f/c/b").next_restart_at = None;
        obs.remove(&id("f/c/b"));
        assert_eq!(
            plan_for(Some(&f), &["f/c/b"], &st, &obs, 5),
            vec!["ensure-crew f/c"]
        );
        // a changed hash while stopped: still nothing (the stop wins)
        let f2 = fleet(&[("a", 1), ("b", 2)]);
        assert_eq!(
            plan_for(Some(&f2), &["f/c/b"], &st, &obs, 5),
            vec!["ensure-crew f/c"]
        );
    }

    #[test]
    fn resuming_a_stopped_agent_restarts_it() {
        let f = fleet(&[("a", 1)]);
        let mut st = FleetStatus::default();
        *st.entry("f/c/a") = AgentStatus {
            phase: AgentPhase::Stopped,
            applied_hash: Some(hash_of(&f, "a")),
            restarts: 3,
            ..AgentStatus::default()
        };
        let got = plan_for(Some(&f), &[], &st, &ObservedState::default(), 0);
        assert_eq!(
            got,
            vec![
                "ensure-crew f/c".to_string(),
                "materialize f/c/a".into(),
                format!("start f/c/a {}", short(&hash_of(&f, "a")))
            ]
        );
    }
}
