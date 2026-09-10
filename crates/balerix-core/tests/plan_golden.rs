//! Plans for five canned situations, rendered one step per line. Review a
//! `.snap.new` against the expected step lists in the Phase 2 plan, Task 7.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use balerix_api::{
    AgentPhase, AgentSettings, AgentStatus, CrewSpec, FleetSpec, FleetStatus, GitSettings,
    Timestamp,
};
use balerix_core::reconcile::{Plan, ReconcilePolicy, plan};
use balerix_core::{AgentId, Fleet, Keep, ObservedState, ProcessState, ResolvedAgent};

fn fleet(agents: &[(&str, &str)]) -> Fleet {
    Fleet::try_from(FleetSpec {
        name: "payments".into(),
        crews: BTreeMap::from([(
            "backend".to_string(),
            CrewSpec {
                repo: "acme/payments-api".into(),
                git_ref: "main".into(),
                git: GitSettings::default(),
                agents: agents
                    .iter()
                    .map(|(n, model)| {
                        let mut s = AgentSettings::default();
                        s.claude.settings = serde_json::json!({ "model": model });
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

fn applied(f: &Fleet, st: &mut FleetStatus, phase: AgentPhase) {
    for a in ResolvedAgent::from_fleet(f) {
        *st.entry(&a.id.to_string()) = AgentStatus {
            phase,
            applied_hash: Some(a.hash()),
            ..AgentStatus::default()
        };
    }
}

fn running(f: &Fleet) -> ObservedState {
    let mut o = ObservedState::default();
    for a in ResolvedAgent::from_fleet(f) {
        o.set(&a.id, ProcessState::Running { pid: 100 });
    }
    o
}

fn render(p: &Plan) -> String {
    // hashes are stable (they derive from fixed settings), so they may appear
    p.iter().map(|s| format!("{s}\n")).collect()
}

fn go(
    desired: Option<&Fleet>,
    keep: Keep,
    stopped: &[&str],
    st: &FleetStatus,
    obs: &ObservedState,
) -> String {
    let stopped: BTreeSet<AgentId> = stopped
        .iter()
        .map(|s| format!("payments/backend/{s}").parse().unwrap())
        .collect();
    render(&plan(
        &"payments".parse().unwrap(),
        desired,
        keep,
        &stopped,
        st,
        obs,
        &ReconcilePolicy::default(),
        Timestamp(1_000),
    ))
}

#[test]
fn fresh_up() {
    let f = fleet(&[("alice", "sonnet"), ("bob", "opus")]);
    insta::assert_snapshot!(
        "fresh_up",
        go(
            Some(&f),
            Keep::default(),
            &[],
            &FleetStatus::default(),
            &ObservedState::default()
        )
    );
}

#[test]
fn one_agent_hash_changed() {
    let before = fleet(&[("alice", "sonnet"), ("bob", "opus")]);
    let after = fleet(&[("alice", "sonnet"), ("bob", "haiku")]);
    let mut st = FleetStatus::default();
    applied(&before, &mut st, AgentPhase::Ready);
    insta::assert_snapshot!(
        "one_agent_hash_changed",
        go(Some(&after), Keep::default(), &[], &st, &running(&before))
    );
}

#[test]
fn one_agent_exited_past_max_restarts() {
    let f = fleet(&[("alice", "sonnet"), ("bob", "opus")]);
    let mut st = FleetStatus::default();
    applied(&f, &mut st, AgentPhase::Ready);
    let mut obs = running(&f);
    obs.set(
        &"payments/backend/bob".parse().unwrap(),
        ProcessState::Exited { code: Some(1) },
    );
    // not yet noted
    let noted_next = go(Some(&f), Keep::default(), &[], &st, &obs);
    // noted, dead
    let bob = st.entry("payments/backend/bob");
    bob.phase = AgentPhase::Dead;
    bob.restarts = 6;
    let dead = go(Some(&f), Keep::default(), &[], &st, &obs);
    insta::assert_snapshot!(
        "one_agent_exited",
        format!("-- unnoted exit --\n{noted_next}-- dead --\n{dead}")
    );
}

#[test]
fn down_keep_repos() {
    let f = fleet(&[("alice", "sonnet"), ("bob", "opus")]);
    let mut st = FleetStatus::default();
    applied(&f, &mut st, AgentPhase::Ready);
    insta::assert_snapshot!(
        "down_keep_repos",
        go(
            None,
            Keep {
                repos: true,
                sessions: false
            },
            &[],
            &st,
            &running(&f)
        )
    );
}

#[test]
fn stopped_agent() {
    let f = fleet(&[("alice", "sonnet"), ("bob", "opus")]);
    let mut st = FleetStatus::default();
    applied(&f, &mut st, AgentPhase::Ready);
    insta::assert_snapshot!(
        "stopped_agent_running",
        go(Some(&f), Keep::default(), &["bob"], &st, &running(&f))
    );
    let mut obs = running(&f);
    obs.remove(&"payments/backend/bob".parse().unwrap());
    st.entry("payments/backend/bob").phase = AgentPhase::Stopped;
    insta::assert_snapshot!(
        "stopped_agent_resumed",
        go(Some(&f), Keep::default(), &[], &st, &obs)
    );
}
