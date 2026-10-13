//! The Agent controller (Spec O §5.4, §8.5, §21.2): once its Crew is
//! ready, the claim, the bundle Secret, the policy and the Pod; a Pod
//! whose `specHash` differs is replaced on the same claim, after a
//! harvest Job has settled the clone when the branch changed; status is the
//! Pod plus the Daemon's record for the agent. On deletion: the Pod
//! gone, the harvest Job (unless purged, or the Fleet goes with
//! `retain: None`, or there is no claim), then the claim.

use std::sync::Arc;

use balerix_api::CredentialBundle;
use futures_util::StreamExt;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Secret};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::{DeleteParams, Patch, PostParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{Error as FinalizerError, Event, finalizer};
use kube::runtime::reflector::ObjectRef;
use kube::runtime::watcher;
use kube::{Api, ResourceExt};

use super::crew::crew_ready;
use super::daemon::read_secret_string;
use super::fleet::daemon_of;
use super::jobs::ensure_job;
use super::owned::{apply_read, delete_read, observe, observe_desired};
use super::{Context, Error, api_in, bounded, error_policy, patch_status, reconciled, report};
use crate::api::{Agent, AgentStatus, Crew, Daemon, Fleet, Retain};
use crate::desired::agent::{
    AgentInputs, BRANCH_ANNOTATION, ClaimBranch, SPEC_HASH_ANNOTATION, agent_branch, agent_objects,
    agent_status, claim_branch,
};
use crate::desired::common::{Cond, HASH_ANNOTATION, JobOutcome, conditions, labels, owner_of};
use crate::desired::fleet::HARVEST_FINALIZER;
use crate::desired::jobs::{BranchMove, JobContext, branch_change_job, harvest_job};
use crate::desired::names;
use crate::desired::ownership::{
    AGENT_LABEL, COMPONENT_LABEL, CREW_LABEL, DAEMON_LABEL, FLEET_LABEL, Identity, identity_of,
};

/// On an Agent: skip its harvest, as `--purge` does (§8.5). On a Fleet,
/// read only while the Fleet is being deleted: skip every Agent's harvest
/// and purge the fleet's record on the Daemon (§25, #135). A Fleet marked
/// from the start still harvests the Agents dropped during its life. Only
/// the value `"true"` counts.
pub const PURGE_ANNOTATION: &str = "balerix.ai/purge";

/// The escape a stuck harvest names, on the Agent and on its Fleet.
pub fn harvest_escape() -> String {
    format!(
        "the annotation {PURGE_ANNOTATION}=true on the Agent skips its harvest; on the Fleet, while the Fleet is deleted, it skips every harvest and also purges the Fleet's record on the Daemon"
    )
}

/// What a branch change waits on (§8.5, §27).
#[derive(Debug, Clone, Copy)]
pub enum BranchWait<'a> {
    /// The old pod, deleted, still listed.
    Pod,
    /// A harvest Job of the Agent's that still runs, whatever it was
    /// made for: the branch was set back while it emptied the clone.
    Running(&'a str),
    /// The harvest Job, not succeeded.
    Job {
        name: &'a str,
        outcome: &'a JobOutcome,
    },
}

/// The escape a blocked branch change names (§27.3). No pod runs while
/// the change is blocked, so the clone's changes are committed or
/// discarded on the old branch.
pub fn branch_change_escape(from: &str) -> String {
    format!(
        "set the branch back to {from}, commit or discard the changes in the pod, then change it again; or annotate the Agent {PURGE_ANNOTATION}=true, which discards the clone's uncommitted changes and unpushed commits"
    )
}

/// The Warning a purged branch change leaves while the annotation stays.
pub fn purge_kept_note(agent: &str) -> String {
    format!(
        "Agent {agent}: its clone was removed unharvested under {PURGE_ANNOTATION}=true; remove the annotation yourself: until you do, every branch change and the Agent's removal discard unpushed work"
    )
}

/// The Agent's `Ready` while its branch change holds the pod (§8.5):
/// `BranchChangeBlocked` once the harvest Job failed, else
/// `BranchChanging` (§27.3). `agent` is `<namespace>/<name>`.
pub fn branch_change_condition(agent: &str, from: &str, to: &str, wait: BranchWait<'_>) -> Cond {
    let changed = format!("Agent {agent}: its branch changed from {from} to {to}");
    match wait {
        BranchWait::Pod => Cond::no(
            "Ready",
            "BranchChanging",
            &format!("{changed}; waiting for its pod to go"),
        ),
        BranchWait::Running(name) => Cond::no(
            "Ready",
            "BranchChanging",
            &format!("{changed}; harvest Job {name} not finished"),
        ),
        BranchWait::Job {
            name,
            outcome: JobOutcome::Failed(message),
        } => Cond::no(
            "Ready",
            "BranchChangeBlocked",
            &format!(
                "{changed}; Job {name} failed: {message}; the clone is kept and no pod starts; {}",
                branch_change_escape(from)
            ),
        ),
        BranchWait::Job { name, .. } => Cond::no(
            "Ready",
            "BranchChanging",
            &format!("{changed}; harvest Job {name} not finished"),
        ),
    }
}

/// The object carries `balerix.ai/purge: "true"`.
pub fn purge_marked(object: &impl ResourceExt) -> bool {
    object
        .annotations()
        .get(PURGE_ANNOTATION)
        .is_some_and(|v| v == "true")
}

pub async fn controller(ctx: Arc<Context>, watches: &kube::Client, namespace: Option<&str>) {
    let client = watches;
    let mapper_ctx = ctx.clone();
    Controller::new(
        api_in::<Agent>(client, namespace),
        watcher::Config::default(),
    )
    .owns(api_in::<Pod>(client, namespace), watcher::Config::default())
    .owns(
        api_in::<PersistentVolumeClaim>(client, namespace),
        watcher::Config::default(),
    )
    .owns(
        api_in::<Secret>(client, namespace),
        watcher::Config::default(),
    )
    .owns(
        api_in::<NetworkPolicy>(client, namespace),
        watcher::Config::default(),
    )
    // a harvest Job has no owner (§26.3): it is mapped by its labels
    .watches(
        api_in::<Job>(client, namespace),
        watcher::Config::default().labels(&format!("{COMPONENT_LABEL}=harvest")),
        |job: Job| {
            let labels = job.labels();
            match (
                labels.get(FLEET_LABEL),
                labels.get(CREW_LABEL),
                labels.get(AGENT_LABEL),
            ) {
                (Some(f), Some(c), Some(a)) => Some(
                    ObjectRef::<Agent>::new(&names::agent(f, c, a))
                        .within(&job.namespace().unwrap_or_default()),
                ),
                _ => None,
            }
        },
    )
    .watches(
        api_in::<Fleet>(client, namespace),
        watcher::Config::default(),
        move |fleet: Fleet| {
            let ns = fleet.namespace().unwrap_or_default();
            let key = Context::key(&ns, &fleet.name_any());
            let agents = mapper_ctx
                .fleet_agents
                .read()
                .unwrap_or_else(|e| e.into_inner());
            agents
                .get(&key)
                .into_iter()
                .flatten()
                .map(|a| ObjectRef::<Agent>::new(a).within(&ns))
                .collect::<Vec<_>>()
        },
    )
    .run(bounded(reconcile), error_policy, ctx.clone())
    .for_each(|r| async move { report("agent", r) })
    .await;
}

pub async fn reconcile(agent: Arc<Agent>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = agent.namespace().unwrap_or_default();
    let agents: Api<Agent> = Api::namespaced(ctx.client.clone(), &namespace);
    let ctx2 = ctx.clone();
    finalizer(&agents, HARVEST_FINALIZER, agent, |event| async move {
        let (agent, result) = match event {
            Event::Apply(agent) => (agent.clone(), apply_agent(agent, ctx2.clone()).await),
            Event::Cleanup(agent) => (agent.clone(), cleanup_agent(agent, ctx2.clone()).await),
        };
        if let Err(Error::Conflict(message)) = &result {
            conflicted(&ctx2, &agent, message).await;
        }
        result
    })
    .await
    .map_err(|e| match e {
        // the reconcile's own error, so `error_policy` sees a `Waiting`
        FinalizerError::ApplyFailed(e) | FinalizerError::CleanupFailed(e) => e,
        other => Error::from(other),
    })
}

/// `Ready=False`, reason `Conflict` (§26): an object by one of this
/// Agent's names is not its own. Logged when it cannot be written.
async fn conflicted(ctx: &Context, agent: &Agent, message: &str) {
    let namespace = agent.namespace().unwrap_or_default();
    let reported = reported(ctx, &namespace, agent);
    let status = with_condition(
        ctx,
        agent,
        None,
        reported.as_ref(),
        Cond::no("Ready", "Conflict", message),
    );
    if let Err(e) = patch_status(&ctx.client, agent, &status).await {
        tracing::warn!(namespace = %namespace, name = %agent.name_any(), "the Conflict condition was not written: {e}");
    }
}

/// The labels of this Agent's objects of `component` (`agent` for the
/// claim, bundle, policy and pod; `token` for its token Secret).
fn agent_labels(agent: &Agent, component: &str) -> serde_json::Value {
    labels(
        &agent.spec.daemon,
        component,
        &[
            (FLEET_LABEL, agent.spec.fleet.as_str()),
            (CREW_LABEL, agent.spec.crew.as_str()),
            (AGENT_LABEL, agent.spec.agent.as_str()),
        ],
    )
}

/// What the Agent's claim carries (§26.1): the Agent's identity and its
/// Daemon's label. The claim has no owner reference and can outlive its
/// Agent (a finalizer removed by hand); its data is only ever the
/// Daemon's it was made under.
fn claim_identity(agent: &Agent) -> Identity {
    let mut id = identity_of(&agent_labels(agent, "agent"));
    id.insert(
        DAEMON_LABEL.to_string(),
        Some(names::daemon_label(&agent.spec.daemon)),
    );
    id
}

/// The Agent's own Pod, when there is one (§26).
async fn own_pod(pods: &Api<Pod>, agent: &Agent) -> Result<Option<Pod>, Error> {
    observe(
        pods,
        &agent.name_any(),
        &identity_of(&agent_labels(agent, "agent")),
        agent.uid().as_deref(),
    )
    .await
}

/// In a cleanup an object by the Agent's name that is not its own is
/// left alone and counts as none: the Agent's own cannot exist beside it.
fn absent_if_foreign<T>(found: Result<Option<T>, Error>, agent: &str) -> Result<Option<T>, Error> {
    match found {
        Err(Error::Conflict(why)) => {
            tracing::warn!(agent = %agent, "{why}");
            Ok(None)
        }
        other => other,
    }
}

/// The Daemon's last word on this agent, from the Fleet's cached record.
fn reported(ctx: &Context, namespace: &str, agent: &Agent) -> Option<balerix_api::AgentStatus> {
    let key = Context::key(namespace, &agent.spec.fleet);
    let records = ctx.records.read().unwrap_or_else(|e| e.into_inner());
    records
        .get(&key)?
        .status
        .agents
        .get(&format!(
            "{}/{}/{}",
            agent.spec.fleet, agent.spec.crew, agent.spec.agent
        ))
        .cloned()
}

/// The credentials the Daemon names (§5.2 step 2), from their Secrets.
/// The inner `Err` is the user's to fix (a Secret or key that is not
/// there, a file that does not parse): it names the Secret and the key,
/// never what the Secret holds.
async fn credentials(
    ctx: &Context,
    namespace: &str,
    daemon: &Daemon,
) -> Result<Result<CredentialBundle, String>, Error> {
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    let mut bundle = CredentialBundle::default();
    if let Some(r) = &daemon.spec.credentials.claude {
        let Some(secret) = secrets.get_opt(&r.secret_name).await? else {
            return Ok(Err(format!(
                "Secret {} (the Daemon's spec.credentials.claude) does not exist",
                r.secret_name
            )));
        };
        let Some(text) = read_secret_string(&secret, "credentials.json") else {
            return Ok(Err(format!(
                "Secret {} has no key credentials.json",
                r.secret_name
            )));
        };
        // serde's message can quote the input: only the position is kept
        match serde_json::from_str(&text) {
            Ok(credentials) => bundle.claude_credentials = Some(credentials),
            Err(e) => {
                return Ok(Err(format!(
                    "Secret {} key credentials.json is not JSON (line {}, column {})",
                    r.secret_name,
                    e.line(),
                    e.column()
                )));
            }
        }
    }
    if let Some(r) = &daemon.spec.credentials.github {
        let Some(secret) = secrets.get_opt(&r.secret_name).await? else {
            return Ok(Err(format!(
                "Secret {} (the Daemon's spec.credentials.github) does not exist",
                r.secret_name
            )));
        };
        let Some(token) = read_secret_string(&secret, "token") else {
            return Ok(Err(format!("Secret {} has no key token", r.secret_name)));
        };
        bundle.gh_token = Some(token.trim_end().to_string());
    }
    Ok(Ok(bundle))
}

/// `agent_status` with one condition replaced (its transition time kept
/// when the status is unchanged, as `conditions` does).
fn with_condition(
    ctx: &Context,
    agent: &Agent,
    pod: Option<&Pod>,
    reported: Option<&balerix_api::AgentStatus>,
    replace: Cond,
) -> AgentStatus {
    let mut status = agent_status(agent, pod, reported, &ctx.k8s_now());
    let old = agent
        .status
        .as_ref()
        .map(|s| s.conditions.as_slice())
        .unwrap_or(&[]);
    for new in conditions(old, &[replace], agent.metadata.generation, &ctx.k8s_now()) {
        match status.conditions.iter_mut().find(|c| c.type_ == new.type_) {
            Some(c) => *c = new,
            None => status.conditions.push(new),
        }
    }
    status
}

async fn apply_agent(agent: Arc<Agent>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = agent.namespace().unwrap_or_default();
    let name = agent.name_any();
    let period = ctx.run.fleet_period;
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let reported = reported(&ctx, &namespace, &agent);

    let Some(daemon) = daemon_of(&ctx, &namespace, &agent.spec.daemon).await? else {
        return Err(Error::Missing(format!(
            "Agent {namespace}/{name}: Daemon {} does not exist",
            agent.spec.daemon
        )));
    };
    let crews: Api<Crew> = Api::namespaced(ctx.client.clone(), &namespace);
    let crew_labels = labels(
        &agent.spec.daemon,
        "crew",
        &[
            (FLEET_LABEL, agent.spec.fleet.as_str()),
            (CREW_LABEL, agent.spec.crew.as_str()),
        ],
    );
    let crew = observe(
        &crews,
        &names::crew(&agent.spec.fleet, &agent.spec.crew),
        &identity_of(&crew_labels),
        None,
    )
    .await?;
    let waiting = match crew.as_ref().map(crew_ready) {
        Some(Ok(())) => None,
        Some(Err(why)) => Some(why),
        None => Some("the Crew does not exist yet".to_string()),
    };
    if let Some(why) = waiting {
        let pod = own_pod(&pods, &agent).await?;
        let status = with_condition(
            &ctx,
            &agent,
            pod.as_ref(),
            reported.as_ref(),
            Cond::unknown("Materialized", "WaitingForCrew", &why),
        );
        patch_status(&ctx.client, agent.as_ref(), &status).await?;
        reconciled(&ctx, agent.as_ref());
        return Ok(Action::requeue(period));
    }

    // a Secret the user must fix is a condition, looked at again each period
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &namespace);
    let token_secret = names::token(&name);
    let token = observe(
        &secrets,
        &token_secret,
        &identity_of(&agent_labels(&agent, "token")),
        None,
    )
    .await?
    .as_ref()
    .and_then(|s| read_secret_string(s, "token"));
    let resolved = match token {
        None => Err(format!(
            "Secret {token_secret} (the agent's Daemon token, minted by the Fleet) does not exist or has no key token"
        )),
        Some(token) => credentials(&ctx, &namespace, &daemon)
            .await?
            .map(|c| (token, c)),
    };
    let (token, credentials) = match resolved {
        Ok(both) => both,
        Err(why) => {
            let pod = own_pod(&pods, &agent).await?;
            let status = with_condition(
                &ctx,
                &agent,
                pod.as_ref(),
                reported.as_ref(),
                Cond::no("Materialized", "CredentialsInvalid", &why),
            );
            patch_status(&ctx.client, agent.as_ref(), &status).await?;
            reconciled(&ctx, agent.as_ref());
            return Ok(Action::requeue(period));
        }
    };
    let objects = agent_objects(&AgentInputs {
        agent: &agent,
        daemon: &daemon,
        token: &token,
        credentials: &credentials,
        cfg: &ctx.cfg,
    })?;

    // every object by one of the Agent's names judged before any is
    // written (§26.1): no Pod while its claim, bundle or policy is in
    // `Conflict`, and the policy before the Pod
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), &namespace);
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &namespace);
    let policies: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &namespace);
    let claim = observe(
        &claims,
        &name,
        &claim_identity(&agent),
        agent.uid().as_deref(),
    )
    .await?;
    let bundle = observe_desired(&secrets, &objects.bundle).await?;
    let policy = observe_desired(&policies, &objects.policy).await?;
    let existing_pod = observe_desired(&pods, &objects.pod).await?;
    apply_read(&policies, &objects.policy, policy.as_ref()).await?;
    apply_read(&secrets, &objects.bundle, bundle.as_ref()).await?;
    // the claim: created when absent; one an earlier operator made owned
    // by the Agent is released, so no garbage collector takes it before
    // the harvest (§26.3)
    match &claim {
        None => {
            apply_read(&claims, &objects.claim, None).await?;
        }
        Some(claim) => release_claim(&claims, claim, &agent).await?,
    }

    // §8.5, §27: a claim whose clone was made for another branch holds the
    // pod until a harvest Job has settled the clone
    if let Some(claim) = &claim
        && let Some(wait) = settle_branch(
            &ctx,
            &agent,
            &claims,
            claim,
            existing_pod.as_ref(),
            reported.as_ref(),
        )
        .await?
    {
        return Ok(wait);
    }

    // the Pod: created when absent, replaced when its specHash differs
    // (§5.4) or it has ended, as an evicted pod has (§11, #234)
    let mut again = period;
    let pod = match existing_pod {
        None => match pods.create(&PostParams::default(), &objects.pod).await {
            Ok(pod) => Some(pod),
            Err(kube::Error::Api(e)) if e.code == 409 => own_pod(&pods, &agent).await?,
            Err(e) => return Err(e.into()),
        },
        Some(pod) if pod.metadata.deletion_timestamp.is_some() => {
            again = std::time::Duration::from_secs(2);
            Some(pod)
        }
        Some(pod) if crate::desired::agent::finished_phase(&pod).is_some() => {
            again = replace_ended(&ctx, &agent, &pods, &pod).await?;
            Some(pod)
        }
        Some(pod) if pod.annotations().get(SPEC_HASH_ANNOTATION) != Some(&agent.spec.spec_hash) => {
            tracing::info!(agent = %name, "specHash changed: replacing the pod");
            delete_read(&pods, &pod, DeleteParams::default()).await?;
            again = std::time::Duration::from_secs(2);
            Some(pod)
        }
        Some(pod) => Some(pod),
    };

    let status = agent_status(&agent, pod.as_ref(), reported.as_ref(), &ctx.k8s_now());
    patch_status(&ctx.client, agent.as_ref(), &status).await?;
    reconciled(&ctx, agent.as_ref());
    Ok(Action::requeue(again))
}

/// §8.5, §27: the Agent's branch against the one its claim's clone was made
/// for. `None` lets the pod be made. Otherwise the pod waits, and the
/// answer is when to look again: the old pod is deleted first (the
/// branch is in the specHash, so it is being replaced anyway), then the
/// harvest Job settles the clone (`harvest --branch`: the old branch
/// into the crew cache and the clone removed, as a host's
/// `ensure_clone` does), and only its success moves the claim's record
/// to the new branch. A failed Job keeps the clone and the pod away,
/// `Ready=False/BranchChangeBlocked`, until it is retried, the branch
/// is set back, or the Agent's `balerix.ai/purge` makes the Job remove
/// the clone unread. A claim with no record (an earlier operator's) is
/// recorded under the Agent's branch, with no harvest.
async fn settle_branch(
    ctx: &Arc<Context>,
    agent: &Agent,
    claims: &Api<PersistentVolumeClaim>,
    claim: &PersistentVolumeClaim,
    pod: Option<&Pod>,
    reported: Option<&balerix_api::AgentStatus>,
) -> Result<Option<Action>, Error> {
    let namespace = agent.namespace().unwrap_or_default();
    let name = agent.name_any();
    let to = agent_branch(&agent.spec)?;
    let who = format!("{namespace}/{name}");
    let from = match claim_branch(claim, &to) {
        ClaimBranch::Changed { from } => from,
        unchanged => {
            if unchanged == ClaimBranch::Unrecorded {
                tracing::info!(agent = %name, branch = %to, "recording the branch of a claim made before §27");
                record_branch(claims, claim, &to).await?;
            }
            // the branch set back while a change's Job empties the clone:
            // no pod until that Job has ended (§27.2)
            let Some(running) = unfinished_harvest(ctx, agent, None).await? else {
                return Ok(None);
            };
            let condition = Cond::no(
                "Ready",
                "BranchChanging",
                &format!("Agent {who}: harvest Job {running} not finished; the pod waits for it"),
            );
            let status = with_condition(ctx, agent, pod, reported, condition);
            patch_status(&ctx.client, agent, &status).await?;
            reconciled(ctx, agent);
            return Ok(Some(Action::requeue(std::time::Duration::from_secs(5))));
        }
    };
    let hold = |wait: BranchWait<'_>| branch_change_condition(&who, &from, &to, wait);
    let wait = |condition: Cond, again: std::time::Duration| async move {
        let status = with_condition(ctx, agent, None, reported, condition);
        patch_status(&ctx.client, agent, &status).await?;
        reconciled(ctx, agent);
        Ok::<_, Error>(Some(Action::requeue(again)))
    };

    // 1. the old pod gone; no new one is made meanwhile
    if let Some(pod) = pod {
        if pod.metadata.deletion_timestamp.is_none() {
            tracing::info!(agent = %name, "branch changed from {from} to {to}: deleting the pod");
            let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
            delete_read(&pods, pod, DeleteParams::default()).await?;
        }
        return wait(hold(BranchWait::Pod), std::time::Duration::from_secs(2)).await;
    }

    // 2. the harvest Job over the claim
    let purge = purge_marked(agent);
    let job_ctx = JobContext {
        namespace: &namespace,
        daemon: &agent.spec.daemon,
        images: &ctx.cfg.images,
        owner: owner_of(agent)?,
    };
    let job = branch_change_job(
        &job_ctx,
        &name,
        &agent.spec,
        &BranchMove {
            from: &from,
            to: &to,
            purge,
        },
    )?;
    let job_name = job.name_any();
    if let Some(running) = unfinished_harvest(ctx, agent, Some(&job)).await? {
        return wait(
            hold(BranchWait::Running(&running)),
            std::time::Duration::from_secs(5),
        )
        .await;
    }
    let ensured = ensure_job(
        ctx.as_ref(),
        agent,
        job,
        Some((&agent.spec.fleet, &agent.spec.crew)),
    )
    .await?;
    match &ensured.outcome {
        JobOutcome::Succeeded(message) => {
            tracing::info!(agent = %name, "branch changed from {from} to {to}: {message}");
            // 3. the record moves; the pod is made on the new branch
            record_branch(claims, claim, &to).await?;
            if purge {
                ctx.warn(agent, "PurgeAnnotationKept", purge_kept_note(&who))
                    .await;
            }
            Ok(None)
        }
        outcome => {
            wait(
                hold(BranchWait::Job {
                    name: &job_name,
                    outcome,
                }),
                ensured.again,
            )
            .await
        }
    }
}

/// The claim's record of its clone's branch (§27.1), by a merge patch
/// over the claim read: its uid is a precondition.
async fn record_branch(
    claims: &Api<PersistentVolumeClaim>,
    claim: &PersistentVolumeClaim,
    branch: &str,
) -> Result<(), Error> {
    claims
        .patch(
            &claim.name_any(),
            &super::owned::merge_params(),
            &Patch::Merge(serde_json::json!({ "metadata": {
                "uid": claim.uid(),
                "annotations": { BRANCH_ANNOTATION: branch },
            } })),
        )
        .await?;
    Ok(())
}

/// The name of the Agent's harvest Job while it runs, or is being
/// deleted with its pod. With `other_than`, only one made from other
/// input: it is let finish before that one replaces it. A branch
/// change's empties the clone; stopped part-way, or overtaken by a pod
/// after the branch was set back, it would leave half a clone (§27.2).
async fn unfinished_harvest(
    ctx: &Context,
    agent: &Agent,
    other_than: Option<&Job>,
) -> Result<Option<String>, Error> {
    let namespace = agent.namespace().unwrap_or_default();
    let name = agent.name_any();
    // the harvest Job's name and identity, whatever it is for
    let probe = harvest_job(
        &JobContext {
            namespace: &namespace,
            daemon: &agent.spec.daemon,
            images: &ctx.cfg.images,
            owner: owner_of(agent)?,
        },
        &name,
        &agent.spec,
    )?;
    let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), &namespace);
    let existing = observe(
        &jobs,
        &probe.name_any(),
        &super::owned::identity_of(&probe),
        agent.uid().as_deref(),
    )
    .await?;
    let Some(job) = existing else {
        return Ok(None);
    };
    match harvest_holds(&job, ctx.now(), ctx.run.stuck_after, other_than) {
        Holds::No => Ok(None),
        Holds::Yes => Ok(Some(job.name_any())),
        Holds::Stuck => {
            tracing::warn!(agent = %name, job = %job.name_any(), "the harvest Job has been deleting past the bound (a lost node?): no longer waited for");
            Ok(None)
        }
    }
}

/// Whether a harvest Job holds the Agent's pod, or a harvest from other
/// input (§27.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holds {
    No,
    Yes,
    /// Being deleted for `stuck_after` or more: its pod is on a lost node
    /// and never finishes deleting (§22.3), so it holds nothing.
    Stuck,
}

/// A Job holds while it runs, or while it is deleted with its pod, up to
/// `stuck_after` past its deletion timestamp: the crew lock's rule for a
/// lost node (`jobs::stuck`). With `other_than`, a Job made from that
/// one's input holds nothing: it is that one. `now` is unix seconds.
pub fn harvest_holds(
    job: &Job,
    now: i64,
    stuck_after: std::time::Duration,
    other_than: Option<&Job>,
) -> Holds {
    let hash = |job: &Job| job.annotations().get(HASH_ANNOTATION).cloned();
    if other_than.is_some_and(|wanted| hash(job) == hash(wanted)) {
        return Holds::No;
    }
    match job.metadata.deletion_timestamp.as_ref() {
        Some(t) if now - t.0.as_second() >= stuck_after.as_secs() as i64 => Holds::Stuck,
        Some(_) => Holds::Yes,
        None if super::jobs::unfinished(job) => Holds::Yes,
        None => Holds::No,
    }
}

/// An ended pod (§11, #234) deleted, so the next reconcile makes it again
/// on the same claim; when to look again. Pods that keep ending young (a
/// kubelet that refuses them at admission, a node that keeps evicting
/// them) wait `replace_delay`, kept on the Agent per pod, before the
/// delete. Each delete is a `PodReplaced` Warning Event on the Agent.
async fn replace_ended(
    ctx: &Context,
    agent: &Agent,
    pods: &Api<Pod>,
    pod: &Pod,
) -> Result<std::time::Duration, Error> {
    use crate::desired::agent::{
        POD_BACKOFF_ANNOTATION, finished_phase, next_backoff, pod_backoff, pod_lifetime,
    };
    use crate::desired::common::DesiredError;
    use kube::api::{Patch, Preconditions};
    let namespace = agent.namespace().unwrap_or_default();
    let name = agent.name_any();
    let now = ctx.now();
    let uid = pod.metadata.uid.clone().unwrap_or_default();
    let backoff = match pod_backoff(agent) {
        Some(b) if b.pod == uid => b,
        previous => {
            let b = next_backoff(previous.as_ref(), &uid, pod_lifetime(pod, now), now);
            let value = serde_json::to_string(&b).map_err(DesiredError::from)?;
            Api::<Agent>::namespaced(ctx.client.clone(), &namespace)
                .patch(
                    &name,
                    &super::owned::merge_params(),
                    &Patch::Merge(serde_json::json!({ "metadata": { "annotations": {
                        POD_BACKOFF_ANNOTATION: value
                    } } })),
                )
                .await?;
            b
        }
    };
    if now < backoff.until {
        tracing::info!(agent = %name, streak = backoff.streak, "the pod ended: replacing it in {} s", backoff.until - now);
        return Ok(std::time::Duration::from_secs((backoff.until - now) as u64));
    }
    let params = DeleteParams {
        preconditions: Some(Preconditions {
            uid: Some(uid),
            resource_version: None,
        }),
        ..DeleteParams::default()
    };
    match pods.delete(&name, &params).await {
        Ok(_) => {}
        // gone, or already another pod: nothing of this one to replace
        Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => {}
        Err(e) => return Err(e.into()),
    }
    let status = pod.status.as_ref();
    let phase = finished_phase(pod).unwrap_or_default();
    let why = [
        status.and_then(|s| s.reason.as_deref()),
        status.and_then(|s| s.message.as_deref()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(": ");
    let mut note = format!("pod {name} ended in phase {phase}");
    if !why.is_empty() {
        note.push_str(&format!(" ({why})"));
    }
    note.push_str("; replaced on the same claim");
    if backoff.streak > 0 {
        note.push_str(&format!(
            " after a {} s back-off ({} in a row ended within {} s of creation)",
            backoff.delay,
            backoff.streak,
            crate::desired::agent::SHORT_LIVED_SECS
        ));
    }
    tracing::info!(agent = %name, "{note}");
    ctx.warn(agent, "PodReplaced", note).await;
    Ok(std::time::Duration::from_secs(2))
}

/// Drops the Agent's owner reference from its claim, which an earlier
/// operator set (§26.3). The claim read is the one patched: its uid is a
/// precondition.
async fn release_claim(
    claims: &Api<PersistentVolumeClaim>,
    claim: &PersistentVolumeClaim,
    agent: &Agent,
) -> Result<(), Error> {
    let uid = agent.uid();
    let owners = claim.owner_references();
    if !owners.iter().any(|o| Some(&o.uid) == uid.as_ref()) {
        return Ok(());
    }
    let kept: Vec<_> = owners
        .iter()
        .filter(|o| Some(&o.uid) != uid.as_ref())
        .collect();
    tracing::info!(agent = %agent.name_any(), "releasing the claim from the Agent's owner reference");
    claims
        .patch(
            &claim.name_any(),
            &super::owned::merge_params(),
            &Patch::Merge(serde_json::json!({
                "metadata": { "uid": claim.uid(), "ownerReferences": kept }
            })),
        )
        .await?;
    Ok(())
}

/// A deletion that waits: the condition written (or the failure to write
/// it logged), and the `Waiting` that requeues the cleanup in 2 s.
async fn deleting(ctx: &Context, agent: &Agent, condition: Cond) -> Error {
    let namespace = agent.namespace().unwrap_or_default();
    let reported = reported(ctx, &namespace, agent);
    let message = condition.message.clone();
    let status = with_condition(ctx, agent, None, reported.as_ref(), condition);
    if let Err(e) = patch_status(&ctx.client, agent, &status).await {
        tracing::warn!(namespace = %namespace, name = %agent.name_any(), "the deletion's condition was not written: {e}");
    }
    Error::Waiting(message)
}

/// The Agent's own mark, or its Fleet's once the Fleet is being deleted
/// (§8.5, §25.3).
fn harvest_skipped(agent: &Agent, fleet: Option<&Fleet>) -> bool {
    purge_marked(agent)
        || fleet.is_some_and(|f| f.metadata.deletion_timestamp.is_some() && purge_marked(f))
}

async fn cleanup_agent(agent: Arc<Agent>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = agent.namespace().unwrap_or_default();
    let name = agent.name_any();
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), &namespace);

    // 1. the Pod gone; one by its name that is not its own is left alone
    let pod = absent_if_foreign(own_pod(&pods, &agent).await, &name)?;
    if let Some(pod) = pod {
        if pod.metadata.deletion_timestamp.is_none() {
            delete_read(&pods, &pod, DeleteParams::default()).await?;
        }
        return Err(deleting(
            &ctx,
            &agent,
            Cond::no(
                "Ready",
                "Deleting",
                &format!("Agent {namespace}/{name}: its pod still exists"),
            ),
        )
        .await);
    }

    // 2. nothing to harvest without a claim of its own, or with one this
    //    finalizer already deleted after its harvest
    let claim = absent_if_foreign(
        observe(
            &claims,
            &name,
            &claim_identity(&agent),
            agent.uid().as_deref(),
        )
        .await,
        &name,
    )?;
    let claim = match claim {
        Some(c) if c.metadata.deletion_timestamp.is_none() => c,
        found => {
            tracing::info!(agent = %name, deleting = found.is_some(), "removed; no claim to harvest");
            return Ok(Action::await_change());
        }
    };
    // a claim an earlier operator made owned by the Agent: released, so
    // the garbage collector cannot take it before the harvest (§26.3)
    release_claim(&claims, &claim, &agent).await?;

    // 3. the harvest, unless skipped (§8.5)
    let fleets: Api<Fleet> = Api::namespaced(ctx.client.clone(), &namespace);
    let fleet = fleets.get_opt(&agent.spec.fleet).await?;
    let fleet_going_whole = fleet
        .as_ref()
        .is_some_and(|f| f.metadata.deletion_timestamp.is_some() && f.spec.retain == Retain::None);
    if !harvest_skipped(&agent, fleet.as_ref()) && !fleet_going_whole {
        let job_ctx = JobContext {
            namespace: &namespace,
            daemon: &agent.spec.daemon,
            images: &ctx.cfg.images,
            owner: owner_of(agent.as_ref())?,
        };
        let job = harvest_job(&job_ctx, &name, &agent.spec)?;
        let job_name = job.name_any();
        let purge = harvest_escape();
        // a branch change's harvest still running is let finish (§27.2)
        if let Some(running) = unfinished_harvest(&ctx, &agent, Some(&job)).await? {
            return Err(deleting(
                &ctx,
                &agent,
                Cond::no(
                    "Ready",
                    "Deleting",
                    &format!(
                        "Agent {namespace}/{name}: harvest Job {running} not finished; {purge}"
                    ),
                ),
            )
            .await);
        }
        let ensured = ensure_job(
            ctx.as_ref(),
            agent.as_ref(),
            job,
            Some((&agent.spec.fleet, &agent.spec.crew)),
        )
        .await?;
        match ensured.outcome {
            JobOutcome::Succeeded(message) => tracing::info!(agent = %name, "{message}"),
            // the namespace takes the claim with it; there is nowhere to harvest to
            _ if ensured.namespace_terminating => {
                tracing::warn!(agent = %name, "not harvested: the namespace is being deleted");
            }
            JobOutcome::Failed(message) => {
                return Err(deleting(
                    &ctx,
                    &agent,
                    Cond::no(
                        "Ready",
                        "HarvestFailed",
                        &format!("Job {job_name} failed: {message}; {purge}"),
                    ),
                )
                .await);
            }
            _ => {
                return Err(deleting(
                    &ctx,
                    &agent,
                    Cond::no(
                        "Ready",
                        "Deleting",
                        &format!(
                            "Agent {namespace}/{name}: harvest Job {job_name} not finished; {purge}"
                        ),
                    ),
                )
                .await);
            }
        }
    }

    // 4. the claim. The harvest Job is left to its TTL (§26.3): a pass
    //    from a cache that still shows the finalizer finds it finished.
    delete_read(&claims, &claim, DeleteParams::default()).await?;
    tracing::info!(agent = %name, "removed");
    Ok(Action::await_change())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_branch_change_says_what_it_waits_on_and_names_the_escape() {
        let escape = "set the branch back to a, commit or discard the changes in the pod, then change it again; or annotate the Agent balerix.ai/purge=true, which discards the clone's uncommitted changes and unpushed commits";
        let pod = branch_change_condition("ns/f-c-x", "a", "b", BranchWait::Pod);
        assert_eq!(
            (pod.status, pod.reason.as_str(), pod.message.as_str()),
            (
                Some(false),
                "BranchChanging",
                "Agent ns/f-c-x: its branch changed from a to b; waiting for its pod to go"
            )
        );
        // a Job that merely runs names no escape
        let running = JobOutcome::Running;
        for wait in [
            BranchWait::Running("f-c-x-harvest"),
            BranchWait::Job {
                name: "f-c-x-harvest",
                outcome: &running,
            },
        ] {
            let c = branch_change_condition("ns/f-c-x", "a", "b", wait);
            assert_eq!(c.reason, "BranchChanging");
            assert_eq!(
                c.message,
                "Agent ns/f-c-x: its branch changed from a to b; harvest Job f-c-x-harvest not finished"
            );
        }
        let failed = JobOutcome::Failed("harvest: f/c/x: dirty".into());
        let c = branch_change_condition(
            "ns/f-c-x",
            "a",
            "b",
            BranchWait::Job {
                name: "f-c-x-harvest",
                outcome: &failed,
            },
        );
        assert_eq!((c.type_, c.status), ("Ready", Some(false)));
        assert_eq!(c.reason, "BranchChangeBlocked");
        assert_eq!(
            c.message,
            format!(
                "Agent ns/f-c-x: its branch changed from a to b; Job f-c-x-harvest failed: harvest: f/c/x: dirty; the clone is kept and no pod starts; {escape}"
            )
        );
        assert!(purge_kept_note("ns/f-c-x").contains("remove the annotation yourself"));
    }

    #[test]
    fn a_harvest_job_holds_while_it_runs_or_deletes_up_to_the_stuck_bound() {
        let job = |hash: &str, status: serde_json::Value, deleting: Option<&str>| -> Job {
            let mut v = serde_json::json!({
                "apiVersion": "batch/v1", "kind": "Job",
                "metadata": { "name": "f-c-x-harvest", "annotations": { HASH_ANNOTATION: hash } },
                "status": status,
            });
            if let Some(t) = deleting {
                v["metadata"]["deletionTimestamp"] = serde_json::json!(t);
            }
            serde_json::from_value(v).unwrap_or_else(|e| panic!("{e}"))
        };
        let bound = std::time::Duration::from_secs(300);
        // 2026-10-03T00:00:00Z
        let deleted_at = 1_790_985_600;
        let running = job("a", serde_json::json!({}), None);
        let done = job("a", serde_json::json!({ "succeeded": 1 }), None);
        let deleting = job(
            "a",
            serde_json::json!({ "succeeded": 1 }),
            Some("2026-10-03T00:00:00Z"),
        );
        assert_eq!(harvest_holds(&running, 0, bound, None), Holds::Yes);
        assert_eq!(harvest_holds(&done, 0, bound, None), Holds::No);
        assert_eq!(
            harvest_holds(&deleting, deleted_at + 299, bound, None),
            Holds::Yes
        );
        // a pod on a lost node never lets its Job go: past the bound, it
        // holds nothing
        assert_eq!(
            harvest_holds(&deleting, deleted_at + 300, bound, None),
            Holds::Stuck
        );
        // the wanted Job itself is ensure_job's to wait on, not a holder
        let same = job("a", serde_json::json!({}), None);
        let other = job("b", serde_json::json!({}), None);
        assert_eq!(harvest_holds(&running, 0, bound, Some(&same)), Holds::No);
        assert_eq!(harvest_holds(&running, 0, bound, Some(&other)), Holds::Yes);
    }
}
