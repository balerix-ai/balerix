//! Walks a `Plan` through the ports (Phase 2 spec §3.3). Dumb on purpose:
//! every decision was made by `plan`, every status change is `apply`.

use std::collections::{BTreeMap, BTreeSet};

use balerix_api::{CredentialBundle, FleetStatus};

use crate::agent::{CrewRef, ResolvedAgent};
use crate::fleet::Fleet;
use crate::name::{AgentId, FleetName};
use crate::ports::{
    AgentRunner, Clock, CrewTools, HookTarget, Keep, LaunchPlan, Materializer, RunnerError,
};
use crate::reconcile::{Plan, ReconcilePolicy, Step, apply, finish_pass, plan};

pub struct ReconcileContext<'a> {
    pub fleet: &'a FleetName,
    pub desired: Option<&'a Fleet>,
    pub keep: Keep,
    pub stopped: &'a BTreeSet<AgentId>,
    pub materializer: &'a dyn Materializer,
    pub runner: &'a dyn AgentRunner,
    pub creds: &'a CredentialBundle,
    pub hooks: &'a dyn Fn(&AgentId) -> HookTarget,
    pub policy: &'a ReconcilePolicy,
    pub clock: &'a dyn Clock,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExecuteReport {
    pub failures: Vec<(Step, String)>,
    pub skipped: Vec<Step>,
}

impl ExecuteReport {
    pub fn all_ok(&self) -> bool {
        self.failures.is_empty() && self.skipped.is_empty()
    }
}

/// Runs every step in order. A failed step for an agent skips that agent's
/// later steps; a failed `EnsureCrew` skips the crew's agents and writes the
/// crew error into each of their messages. Ends with `finish_pass`.
pub fn execute(plan: &Plan, status: &mut FleetStatus, ctx: &ReconcileContext) -> ExecuteReport {
    let agents: BTreeMap<AgentId, ResolvedAgent> = ctx
        .desired
        .map(|f| {
            ResolvedAgent::from_fleet(f)
                .into_iter()
                .map(|a| (a.id.clone(), a))
                .collect()
        })
        .unwrap_or_default();
    let mut report = ExecuteReport::default();
    let mut failed_agents: Vec<AgentId> = Vec::new();
    let mut failed_crews: BTreeMap<CrewRef, String> = BTreeMap::new();
    let mut plans: BTreeMap<AgentId, LaunchPlan> = BTreeMap::new();

    for step in plan {
        if let Some(id) = step.agent_id() {
            if failed_agents.contains(id) {
                report.skipped.push(step.clone());
                continue;
            }
            if let Some(err) = failed_crews.get(&id.crew_ref()) {
                status.entry(&id.to_string()).message.clone_from(err);
                report.skipped.push(step.clone());
                continue;
            }
        }
        let now = ctx.clock.now();
        let outcome: Result<(), String> = match step {
            Step::Stop(id) => ctx.runner.stop_agent(id).map_err(|e| e.to_string()),
            Step::RemoveAgent(id) => ctx.materializer.remove_agent(id).map_err(|e| e.to_string()),
            Step::RemoveCrew(crew, keep) => ctx
                .runner
                .stop_crew(crew)
                .map_err(|e| e.to_string())
                .and_then(|()| {
                    ctx.materializer
                        .remove_crew(crew, *keep)
                        .map_err(|e| e.to_string())
                }),
            Step::EnsureCrew(crew) => ensure_crew(ctx, crew),
            Step::Materialize(id) => match agents.get(id) {
                Some(agent) => ctx
                    .materializer
                    .materialize(agent, ctx.creds, &(ctx.hooks)(id))
                    .map(|p| {
                        plans.insert(id.clone(), p);
                    })
                    .map_err(|e| e.to_string()),
                None => Err(format!("{id}: not in the desired fleet")),
            },
            Step::Start(id, _) => match plans.get(id) {
                Some(p) => ctx.runner.ensure_agent(id, p).map_err(|e| e.to_string()),
                None => Err(format!("{id}: no launch plan (materialize did not run)")),
            },
            Step::NoteExit(..) => Ok(()),
        };
        apply(status, step, &outcome, ctx.policy, now);
        if let Err(e) = outcome {
            match step {
                Step::EnsureCrew(crew) => {
                    failed_crews.insert(crew.clone(), e.clone());
                }
                _ => {
                    if let Some(id) = step.agent_id() {
                        failed_agents.push(id.clone());
                    }
                }
            }
            report.failures.push((step.clone(), e));
        }
    }
    finish_pass(status, ctx.desired.is_none(), report.all_ok());
    report
}

fn ensure_crew(ctx: &ReconcileContext, crew: &CrewRef) -> Result<(), String> {
    let fleet = ctx
        .desired
        .ok_or_else(|| format!("{crew}: not in the desired fleet"))?;
    let desired = fleet
        .crews
        .get(&crew.crew)
        .ok_or_else(|| format!("{crew}: not in the desired fleet"))?;
    ctx.materializer
        .ensure_crew(
            crew,
            &desired.repo,
            &desired.git_ref,
            &desired.git,
            ctx.creds,
            CrewTools {
                fleet: &fleet.tools,
                crew: &desired.tools,
            },
        )
        .map_err(|e| e.to_string())?;
    ctx.runner.ensure_crew(crew).map_err(|e| e.to_string())
}

/// One full pass: observe, plan, execute.
pub fn reconcile_pass(
    status: &mut FleetStatus,
    ctx: &ReconcileContext,
) -> Result<(Plan, ExecuteReport), RunnerError> {
    let observed = ctx.runner.observe(ctx.fleet)?;
    let p = plan(
        ctx.fleet,
        ctx.desired,
        ctx.keep,
        ctx.stopped,
        status,
        &observed,
        ctx.policy,
        ctx.clock.now(),
    );
    let report = execute(&p, status, ctx);
    Ok((p, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::{FakeClock, FakeMaterializer, FakeRunner};
    use balerix_api::{
        AgentPhase, AgentSettings, CrewSpec, FleetPhase, FleetSpec, GitSettings, Timestamp,
    };

    fn fleet(agents: &[&str]) -> Fleet {
        Fleet::try_from(FleetSpec {
            name: "f".into(),
            crews: std::collections::BTreeMap::from([(
                "c".to_string(),
                CrewSpec {
                    repo: "acme/api".into(),
                    git_ref: "main".into(),
                    git: GitSettings::default(),
                    agents: agents
                        .iter()
                        .map(|a| (a.to_string(), AgentSettings::default()))
                        .collect(),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        })
        .unwrap()
    }

    fn hooks(id: &AgentId) -> HookTarget {
        HookTarget {
            url: "https://127.0.0.1:7643".into(),
            secret: format!("s-{id}"),
        }
    }

    struct Harness {
        m: FakeMaterializer,
        r: FakeRunner,
        clock: FakeClock,
        creds: CredentialBundle,
        policy: ReconcilePolicy,
        fleet_name: FleetName,
        stopped: BTreeSet<AgentId>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                m: FakeMaterializer::default(),
                r: FakeRunner::default(),
                clock: FakeClock::new(Timestamp(1000)),
                creds: CredentialBundle::default(),
                policy: ReconcilePolicy::default(),
                fleet_name: "f".parse().unwrap(),
                stopped: BTreeSet::new(),
            }
        }
        fn ctx<'a>(&'a self, desired: Option<&'a Fleet>) -> ReconcileContext<'a> {
            ReconcileContext {
                fleet: &self.fleet_name,
                desired,
                keep: Keep::default(),
                stopped: &self.stopped,
                materializer: &self.m,
                runner: &self.r,
                creds: &self.creds,
                hooks: &hooks,
                policy: &self.policy,
                clock: &self.clock,
            }
        }
    }

    fn harness_with_tools(
        fleet_tools: BTreeMap<String, String>,
        crew_tools: BTreeMap<String, String>,
    ) -> (Harness, Fleet) {
        let h = Harness::new();
        let mut f = fleet(&["a"]);
        f.tools = fleet_tools;
        if let Some(c) = f.crews.get_mut(&"c".parse().unwrap()) {
            c.tools = crew_tools;
        }
        (h, f)
    }

    #[test]
    fn ensure_crew_passes_the_fleet_and_crew_tool_tables_through() {
        let (h, f) = harness_with_tools(
            BTreeMap::from([("node".to_string(), "22.11.0".to_string())]),
            BTreeMap::from([("python".to_string(), "3.12.8".to_string())]),
        );
        let seen = h.m.crew_tools();
        assert!(seen.is_empty(), "nothing recorded before the pass");
        let mut st = FleetStatus::default();
        let _ = execute(
            &vec![Step::EnsureCrew("f/c".parse().unwrap())],
            &mut st,
            &h.ctx(Some(&f)),
        );
        assert_eq!(
            h.m.crew_tools(),
            vec![(
                "f/c".to_string(),
                vec!["node=22.11.0".to_string()],
                vec!["python=3.12.8".to_string()]
            )],
            "the materializer sees each level's own table, unmerged"
        );
    }

    #[test]
    fn a_pass_brings_a_fresh_fleet_to_starting() {
        let h = Harness::new();
        let f = fleet(&["a", "b"]);
        let mut st = FleetStatus::default();
        let (p, rep) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert!(rep.all_ok(), "{rep:?}");
        assert_eq!(p.len(), 5);
        assert_eq!(st.agents["f/c/a"].phase, AgentPhase::Starting);
        assert_eq!(st.agents["f/c/b"].phase, AgentPhase::Starting);
        assert_eq!(st.phase, FleetPhase::Reconciling);
        assert_eq!(st.observed_generation, st.generation);
        assert_eq!(
            h.m.calls(),
            vec!["ensure_crew f/c", "materialize f/c/a", "materialize f/c/b"]
        );
        assert_eq!(
            h.r.calls(),
            vec![
                "observe f",
                "ensure_crew f/c",
                "ensure_agent f/c/a",
                "ensure_agent f/c/b"
            ]
        );
        // second pass: nothing but ensure-crew
        let (p2, _) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert_eq!(
            p2.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["ensure-crew f/c"]
        );
    }

    #[test]
    fn a_failed_materialize_skips_start_and_degrades_the_fleet() {
        let h = Harness::new();
        h.m.fail_next("materialize", "f/c/a", "no space left");
        let f = fleet(&["a", "b"]);
        let mut st = FleetStatus::default();
        let (_, rep) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert_eq!(rep.failures.len(), 1);
        assert_eq!(
            rep.skipped
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .len(),
            1
        );
        assert_eq!(st.agents["f/c/a"].phase, AgentPhase::Failed);
        assert_eq!(
            st.agents["f/c/a"].message,
            "f/c/a: mise materialize: no space left"
        );
        assert_eq!(
            st.agents["f/c/b"].phase,
            AgentPhase::Starting,
            "other agents proceed"
        );
        assert_eq!(st.phase, FleetPhase::Degraded);
        assert_ne!(st.observed_generation, 1);
        assert!(!h.r.calls().contains(&"ensure_agent f/c/a".to_string()));
        // next pass retries and succeeds
        let (_, rep) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert!(rep.all_ok());
        assert_eq!(st.agents["f/c/a"].phase, AgentPhase::Starting);
    }

    #[test]
    fn a_failed_ensure_crew_skips_every_agent_of_that_crew() {
        let h = Harness::new();
        h.m.fail_next("ensure_crew", "f/c", "clone failed");
        let f = fleet(&["a"]);
        let mut st = FleetStatus::default();
        let (_, rep) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert_eq!(rep.failures.len(), 1);
        assert_eq!(rep.skipped.len(), 2);
        assert_eq!(
            st.agents["f/c/a"].message,
            "f/c: git ensure_crew: clone failed"
        );
        assert!(h.m.calls().iter().all(|c| !c.starts_with("materialize")));
    }

    #[test]
    fn down_stops_and_removes_then_down() {
        let h = Harness::new();
        let f = fleet(&["a"]);
        let mut st = FleetStatus::default();
        reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        let ctx = ReconcileContext {
            keep: Keep {
                repos: true,
                sessions: true,
            },
            ..h.ctx(None)
        };
        let (p, rep) = reconcile_pass(&mut st, &ctx).unwrap();
        assert!(rep.all_ok());
        assert_eq!(
            p.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec![
                "stop f/c/a",
                "remove-crew f/c keep-repos=true keep-sessions=true"
            ]
        );
        assert!(st.agents.is_empty());
        assert_eq!(st.phase, FleetPhase::Down);
        assert!(
            h.m.calls()
                .contains(&"remove_crew f/c repos=true sessions=true".to_string())
        );
        assert!(h.r.observed().crews.is_empty());
    }

    #[test]
    fn an_exit_is_noted_then_restarted_after_backoff() {
        let h = Harness::new();
        let f = fleet(&["a"]);
        let mut st = FleetStatus::default();
        reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        h.r.set_state(
            &"f/c/a".parse().unwrap(),
            crate::ports::ProcessState::Exited { code: Some(1) },
        );
        reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert_eq!(st.agents["f/c/a"].restarts, 1);
        assert_eq!(st.agents["f/c/a"].next_restart_at, Some(Timestamp(1002)));
        let (p, _) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert_eq!(p.len(), 1, "backoff not elapsed: only ensure-crew");
        h.clock.advance(2);
        let (p, _) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(st.agents["f/c/a"].phase, AgentPhase::Starting);
        assert_eq!(
            h.r.observed().get(&"f/c/a".parse().unwrap()),
            Some(&crate::ports::ProcessState::Running { pid: 2 })
        );
    }

    #[test]
    fn stop_then_resume_keeps_the_restart_count() {
        let mut h = Harness::new();
        let f = fleet(&["a"]);
        let mut st = FleetStatus::default();
        reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        st.agents.get_mut("f/c/a").unwrap().restarts = 2;
        h.stopped.insert("f/c/a".parse().unwrap());
        let (p, rep) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert!(rep.all_ok());
        assert_eq!(
            p.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["stop f/c/a", "ensure-crew f/c"]
        );
        assert_eq!(st.agents["f/c/a"].phase, AgentPhase::Stopped);
        assert_eq!(h.r.observed().get(&"f/c/a".parse().unwrap()), None);
        // idle while stopped
        let (p, _) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert_eq!(p.len(), 1);
        h.stopped.clear();
        let (p, _) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(st.agents["f/c/a"].phase, AgentPhase::Starting);
        assert_eq!(
            st.agents["f/c/a"].restarts, 2,
            "a deliberate stop is not an exit"
        );
    }
}
