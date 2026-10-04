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
use kube::api::{DeleteParams, PostParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{Error as FinalizerError, Event, finalizer};
use kube::runtime::reflector::ObjectRef;
use kube::runtime::watcher;
use kube::{Api, ResourceExt};

use super::crew::crew_ready;
use super::daemon::read_secret_string;
use super::fleet::daemon_of;
use super::jobs::ensure_job;
use super::{Context, Error, api_in, apply, error_policy, patch_status, reconciled, report};
use crate::api::{Agent, AgentStatus, Crew, Daemon, Fleet, Retain};
use crate::desired::agent::{AgentInputs, SPEC_HASH_ANNOTATION, agent_objects, agent_status};
use crate::desired::common::{Cond, JobOutcome, conditions, owner_of};
use crate::desired::fleet::HARVEST_FINALIZER;
use crate::desired::jobs::{JobContext, harvest_job};
use crate::desired::names;

/// On an Agent or its Fleet: skip the harvest, as `--purge` does (§8.5).
pub const PURGE_ANNOTATION: &str = "balerix.ai/purge";

pub async fn controller(ctx: Arc<Context>, namespace: Option<&str>) {
    let client = &ctx.client;
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
    .owns(api_in::<Job>(client, namespace), watcher::Config::default())
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
    .run(reconcile, error_policy, ctx.clone())
    .for_each(|r| async move { report("agent", r) })
    .await;
}

pub async fn reconcile(agent: Arc<Agent>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = agent.namespace().unwrap_or_default();
    let agents: Api<Agent> = Api::namespaced(ctx.client.clone(), &namespace);
    let ctx2 = ctx.clone();
    finalizer(&agents, HARVEST_FINALIZER, agent, |event| async move {
        match event {
            Event::Apply(agent) => apply_agent(agent, ctx2).await,
            Event::Cleanup(agent) => cleanup_agent(agent, ctx2).await,
        }
    })
    .await
    .map_err(|e| match e {
        // the reconcile's own error, so `error_policy` sees a `Waiting`
        FinalizerError::ApplyFailed(e) | FinalizerError::CleanupFailed(e) => e,
        other => Error::from(other),
    })
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
    let crew = crews
        .get_opt(&names::crew(&agent.spec.fleet, &agent.spec.crew))
        .await?;
    let waiting = match crew.as_ref().map(crew_ready) {
        Some(Ok(())) => None,
        Some(Err(why)) => Some(why),
        None => Some("the Crew does not exist yet".to_string()),
    };
    if let Some(why) = waiting {
        let pod = pods.get_opt(&name).await?;
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
    let token = secrets
        .get_opt(&token_secret)
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
            let pod = pods.get_opt(&name).await?;
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

    let claims: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), &namespace);
    if claims.get_opt(&name).await?.is_none() {
        claims
            .create(&PostParams::default(), &objects.claim)
            .await?;
    }
    apply(&ctx.client, &objects.bundle).await?;
    apply(&ctx.client, &objects.policy).await?;

    // the Pod: created when absent, replaced when its specHash differs (§5.4)
    let mut again = period;
    let pod = match pods.get_opt(&name).await? {
        None => match pods.create(&PostParams::default(), &objects.pod).await {
            Ok(pod) => Some(pod),
            Err(kube::Error::Api(e)) if e.code == 409 => pods.get_opt(&name).await?,
            Err(e) => return Err(e.into()),
        },
        Some(pod) if pod.metadata.deletion_timestamp.is_some() => {
            again = std::time::Duration::from_secs(2);
            Some(pod)
        }
        Some(pod) if pod.annotations().get(SPEC_HASH_ANNOTATION) != Some(&agent.spec.spec_hash) => {
            tracing::info!(agent = %name, "specHash changed: replacing the pod");
            pods.delete(&name, &DeleteParams::default()).await?;
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

fn purged(agent: &Agent, fleet: Option<&Fleet>) -> bool {
    let marked = |a: &std::collections::BTreeMap<String, String>| {
        a.get(PURGE_ANNOTATION).is_some_and(|v| v == "true")
    };
    marked(agent.annotations()) || fleet.is_some_and(|f| marked(f.annotations()))
}

async fn cleanup_agent(agent: Arc<Agent>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = agent.namespace().unwrap_or_default();
    let name = agent.name_any();
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), &namespace);

    // 1. the Pod gone
    if let Some(pod) = pods.get_opt(&name).await? {
        if pod.metadata.deletion_timestamp.is_none() {
            pods.delete(&name, &DeleteParams::default()).await?;
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

    // 2. nothing to harvest without a claim
    if claims.get_opt(&name).await?.is_none() {
        tracing::info!(agent = %name, "removed; it never had a claim");
        return Ok(Action::await_change());
    }

    // 3. the harvest, unless skipped (§8.5)
    let fleets: Api<Fleet> = Api::namespaced(ctx.client.clone(), &namespace);
    let fleet = fleets.get_opt(&agent.spec.fleet).await?;
    let fleet_going_whole = fleet
        .as_ref()
        .is_some_and(|f| f.metadata.deletion_timestamp.is_some() && f.spec.retain == Retain::None);
    if !purged(&agent, fleet.as_ref()) && !fleet_going_whole {
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
        let purge = format!(
            "the annotation {PURGE_ANNOTATION}=true on the Agent or its Fleet skips the harvest"
        );
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

    // 4. the claim
    claims.delete(&name, &DeleteParams::default()).await?;
    tracing::info!(agent = %name, "removed");
    Ok(Action::await_change())
}
