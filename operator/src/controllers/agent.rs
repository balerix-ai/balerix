//! The Agent controller (Spec O §5.4, §8.5, §21.2): once its Crew is
//! ready, the claim, the bundle Secret, the policy and the Pod; a Pod
//! whose `specHash` differs is replaced on the same claim; status is the
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
use crate::desired::agent::{AgentInputs, SPEC_HASH_ANNOTATION, agent_objects, agent_status};
use crate::desired::common::{Cond, JobOutcome, conditions, labels, owner_of};
use crate::desired::fleet::HARVEST_FINALIZER;
use crate::desired::jobs::{JobContext, harvest_job};
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
    match claim {
        None => {
            apply_read(&claims, &objects.claim, None).await?;
        }
        Some(claim) => release_claim(&claims, &claim, &agent).await?,
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
        let ensured = ensure_job(
            ctx.as_ref(),
            agent.as_ref(),
            job,
            Some((&agent.spec.fleet, &agent.spec.crew)),
        )
        .await?;
        let purge = harvest_escape();
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
