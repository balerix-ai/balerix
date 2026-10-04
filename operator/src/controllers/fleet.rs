//! The Fleet controller (Spec O §5.2, §21.2): resolve, mint the missing
//! agent tokens, `PUT` the Daemon, then the children; a rejection lands
//! nothing. The record it reads back is the status the Agents mirror. On
//! deletion it waits for the Agents, downs the fleet on the Daemon and,
//! for `retain: None`, runs one cleanup Job per crew.

use std::sync::Arc;

use balerix_api::AgentTokens;
use futures_util::StreamExt;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{DeleteParams, ListParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{Error as FinalizerError, Event, finalizer};
use kube::runtime::reflector::ObjectRef;
use kube::runtime::watcher;
use kube::{Api, ResourceExt};

use super::agent::PURGE_ANNOTATION;
use super::daemon::{authority_and_token, read_secret_string};
use super::jobs::ensure_job;
use super::{
    Context, Error, api_in, apply, error_policy, patch_status, reconciled, replace_condition,
    report,
};
use crate::api::{Agent, Crew, Daemon, Fleet, FleetStatus, Retain};
use crate::daemon_client::{ClientError, DaemonClient};
use crate::desired::common::{Cond, JobOutcome, conditions, labels, owner_of, typed};
use crate::desired::fleet::{Accepted, FLEET_FINALIZER, FleetPlan, fleet_conditions, plan_fleet};
use crate::desired::jobs::{JobContext, fleet_pool_job, remove_job};
use crate::desired::names;
use crate::pki::new_token;

pub async fn controller(ctx: Arc<Context>, namespace: Option<&str>) {
    let client = &ctx.client;
    let mapper_ctx = ctx.clone();
    Controller::new(
        api_in::<Fleet>(client, namespace),
        watcher::Config::default(),
    )
    .owns(
        api_in::<Crew>(client, namespace),
        watcher::Config::default(),
    )
    .owns(
        api_in::<Agent>(client, namespace),
        watcher::Config::default(),
    )
    .owns(api_in::<Job>(client, namespace), watcher::Config::default())
    .watches(
        api_in::<Daemon>(client, namespace),
        watcher::Config::default(),
        move |daemon: Daemon| {
            let ns = daemon.namespace().unwrap_or_default();
            let key = Context::key(&ns, &daemon.name_any());
            let fleets = mapper_ctx
                .daemon_fleets
                .read()
                .unwrap_or_else(|e| e.into_inner());
            fleets
                .get(&key)
                .into_iter()
                .flatten()
                .map(|f| ObjectRef::<Fleet>::new(f).within(&ns))
                .collect::<Vec<_>>()
        },
    )
    .run(reconcile, error_policy, ctx.clone())
    .for_each(|r| async move { report("fleet", r) })
    .await;
}

pub async fn daemon_of(
    ctx: &Context,
    namespace: &str,
    name: &str,
) -> Result<Option<Daemon>, Error> {
    Ok(Api::<Daemon>::namespaced(ctx.client.clone(), namespace)
        .get_opt(name)
        .await?)
}

/// The client for a Fleet's Daemon; `Missing` until the Daemon controller
/// has minted the authority and token.
pub async fn client_for(ctx: &Context, daemon: &Daemon) -> Result<Arc<DaemonClient>, Error> {
    let namespace = daemon.namespace().unwrap_or_default();
    let name = daemon.name_any();
    let (authority, token) = authority_and_token(ctx, &namespace, &name).await?;
    let endpoint = daemon
        .status
        .as_ref()
        .and_then(|s| s.endpoint.clone())
        .unwrap_or_else(|| names::endpoint(&namespace, &name));
    ctx.daemon_client(&namespace, &name, &endpoint, &authority, &token)
}

pub async fn reconcile(fleet: Arc<Fleet>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = fleet.namespace().unwrap_or_default();
    let fleets: Api<Fleet> = Api::namespaced(ctx.client.clone(), &namespace);
    let ctx2 = ctx.clone();
    finalizer(&fleets, FLEET_FINALIZER, fleet, |event| async move {
        match event {
            Event::Apply(fleet) => apply_fleet(fleet, ctx2).await,
            Event::Cleanup(fleet) => {
                let result = cleanup_fleet(fleet.clone(), ctx2.clone()).await;
                if let Err(e) = &result {
                    deleting(&ctx2, &fleet, e).await;
                }
                result
            }
        }
    })
    .await
    .map_err(|e| match e {
        // the reconcile's own error, so `error_policy` sees a `Waiting`
        FinalizerError::ApplyFailed(e) | FinalizerError::CleanupFailed(e) => e,
        other => Error::from(other),
    })
}

/// The tokens of every planned Agent, minted into a Secret owned by the
/// Fleet when absent (§5.2 step 3).
async fn ensure_tokens(
    ctx: &Context,
    fleet: &Fleet,
    plan: &FleetPlan,
) -> Result<AgentTokens, Error> {
    let namespace = fleet.namespace().unwrap_or_default();
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &namespace);
    let mut tokens = AgentTokens::new();
    for agent in &plan.agents {
        let key = format!(
            "{}/{}/{}",
            agent.spec.fleet, agent.spec.crew, agent.spec.agent
        );
        let name = names::token(&agent.name_any());
        let token = match secrets
            .get_opt(&name)
            .await?
            .as_ref()
            .and_then(|s| read_secret_string(s, "token"))
        {
            Some(t) => t,
            None => {
                let token = new_token();
                let secret: Secret = typed(serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "Secret",
                    "metadata": {
                        "name": name,
                        "namespace": namespace,
                        "labels": labels(&agent.spec.daemon, "token", &[
                            ("balerix.ai/fleet", agent.spec.fleet.as_str()),
                            ("balerix.ai/crew", agent.spec.crew.as_str()),
                            ("balerix.ai/agent", agent.spec.agent.as_str()),
                        ]),
                        "ownerReferences": [owner_of(fleet)?],
                    },
                    "type": "Opaque",
                    "stringData": { "token": token },
                }))?;
                apply(&ctx.client, &secret).await?;
                token
            }
        };
        tokens.insert(key, token);
    }
    Ok(tokens)
}

async fn write_status(
    ctx: &Context,
    fleet: &Fleet,
    resolved: Result<(), String>,
    accepted: &Accepted,
    ready: usize,
    total: usize,
) -> Result<(), Error> {
    let old = fleet
        .status
        .as_ref()
        .map(|s| s.conditions.as_slice())
        .unwrap_or(&[]);
    let status = FleetStatus {
        observed_generation: fleet.metadata.generation,
        conditions: conditions(
            old,
            &fleet_conditions(resolved, accepted, ready, total),
            fleet.metadata.generation,
            &ctx.k8s_now(),
        ),
    };
    patch_status(&ctx.client, fleet, &status).await
}

/// Agents of this fleet the plan has (`wanted`) whose `Ready` condition
/// is true: a dropped Agent still deleting is not counted against the
/// plan's total.
async fn ready_agents(
    ctx: &Context,
    namespace: &str,
    fleet: &str,
    wanted: &[String],
) -> Result<usize, Error> {
    let agents: Api<Agent> = Api::namespaced(ctx.client.clone(), namespace);
    let list = agents
        .list(&ListParams::default().labels(&format!("balerix.ai/fleet={fleet}")))
        .await?;
    Ok(list
        .items
        .iter()
        .filter(|a| wanted.contains(&a.name_any()))
        .filter(|a| {
            a.status.as_ref().is_some_and(|s| {
                s.conditions
                    .iter()
                    .any(|c| c.type_ == "Ready" && c.status == "True")
            })
        })
        .count())
}

async fn apply_fleet(fleet: Arc<Fleet>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = fleet.namespace().unwrap_or_default();
    let name = fleet.name_any();
    let key = Context::key(&namespace, &name);
    let period = ctx.run.fleet_period;

    let Some(daemon) = daemon_of(&ctx, &namespace, &fleet.spec.daemon).await? else {
        let message = format!("spec.daemon: Daemon {} does not exist", fleet.spec.daemon);
        write_status(&ctx, &fleet, Err(message), &Accepted::NotAttempted, 0, 0).await?;
        return Ok(Action::requeue(period));
    };
    {
        let mut by_daemon = ctx.daemon_fleets.write().unwrap_or_else(|e| e.into_inner());
        let list = by_daemon
            .entry(Context::key(&namespace, &fleet.spec.daemon))
            .or_default();
        if !list.contains(&name) {
            list.push(name.clone());
        }
    }

    // resolve, then the tokens, then the plan with every token in it
    let first = match plan_fleet(&fleet, &daemon, &AgentTokens::new(), &ctx.cfg.images) {
        Ok(plan) => plan,
        Err(e) => {
            write_status(
                &ctx,
                &fleet,
                Err(e.to_string()),
                &Accepted::NotAttempted,
                0,
                0,
            )
            .await?;
            reconciled(&ctx, fleet.as_ref());
            return Ok(Action::requeue(period));
        }
    };
    let tokens = ensure_tokens(&ctx, &fleet, &first).await?;
    let plan = plan_fleet(&fleet, &daemon, &tokens, &ctx.cfg.images)?;
    let request = plan.request.as_ref().ok_or_else(|| {
        Error::Missing(format!(
            "Fleet {name}: a token is still missing after minting"
        ))
    })?;

    // the Daemon: a 400 lands nothing (§5.2 step 4)
    let accepted = match client_for(&ctx, &daemon).await {
        Err(Error::Missing(why)) => Accepted::DaemonUnavailable(why),
        Err(e) => return Err(e),
        Ok(client) => match client.apply(request).await {
            Ok(_) => Accepted::Yes,
            Err(ClientError::Rejected(m)) | Err(ClientError::Conflict(m)) => Accepted::Rejected(m),
            Err(e) => Accepted::DaemonUnavailable(e.to_string()),
        },
    };
    let total = plan.agents.len();
    let wanted_agents: Vec<String> = plan.agents.iter().map(ResourceExt::name_any).collect();
    if accepted != Accepted::Yes {
        let ready = ready_agents(&ctx, &namespace, &name, &wanted_agents).await?;
        write_status(&ctx, &fleet, Ok(()), &accepted, ready, total).await?;
        reconciled(&ctx, fleet.as_ref());
        return Ok(Action::requeue(period));
    }

    // the children
    let job_ctx = JobContext {
        namespace: &namespace,
        daemon: &fleet.spec.daemon,
        images: &ctx.cfg.images,
        owner: owner_of(fleet.as_ref())?,
    };
    let pool = ensure_job(
        ctx.as_ref(),
        fleet.as_ref(),
        fleet_pool_job(&job_ctx, &name, &plan.spec.tools)?,
        None,
    )
    .await?;
    for crew in &plan.crews {
        apply(&ctx.client, crew).await?;
    }
    for agent in &plan.agents {
        apply(&ctx.client, agent).await?;
    }
    let crews: Api<Crew> = Api::namespaced(ctx.client.clone(), &namespace);
    let agents: Api<Agent> = Api::namespaced(ctx.client.clone(), &namespace);
    let selector = ListParams::default().labels(&format!("balerix.ai/fleet={name}"));
    let wanted_crews: Vec<String> = plan.crews.iter().map(ResourceExt::name_any).collect();
    for crew in crews.list(&selector).await?.items {
        if !wanted_crews.contains(&crew.name_any()) && crew.metadata.deletion_timestamp.is_none() {
            tracing::info!(fleet = %key, crew = %crew.name_any(), "dropped from the Fleet");
            crews
                .delete(&crew.name_any(), &DeleteParams::default())
                .await?;
        }
    }
    for agent in agents.list(&selector).await?.items {
        if !wanted_agents.contains(&agent.name_any()) && agent.metadata.deletion_timestamp.is_none()
        {
            tracing::info!(fleet = %key, agent = %agent.name_any(), "dropped from the Fleet");
            agents
                .delete(&agent.name_any(), &DeleteParams::default())
                .await?;
        }
    }
    ctx.fleet_agents
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.clone(), wanted_agents.clone());

    // the record the Agents mirror (§21.1)
    if let Ok(client) = client_for(&ctx, &daemon).await
        && let Some(record) = client.get(&name).await?
    {
        ctx.records
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), record);
    }
    let ready = ready_agents(&ctx, &namespace, &name, &wanted_agents).await?;
    write_status(&ctx, &fleet, Ok(()), &accepted, ready, total).await?;
    reconciled(&ctx, fleet.as_ref());
    Ok(Action::requeue(period.min(pool.again)))
}

/// While the Fleet's deletion waits or fails: `Ready=False`, reason
/// `Deleting`, with why, the other conditions as they were. A status
/// that cannot be written is logged: the cleanup's own error is the one
/// `error_policy` sees.
async fn deleting(ctx: &Context, fleet: &Fleet, why: &Error) {
    let message = match why {
        Error::Waiting(m) => m.clone(),
        // the one Daemon call of a cleanup
        Error::Daemon(e) => format!(
            "Fleet {}: Daemon {} did not take the down: {e}",
            fleet.name_any(),
            fleet.spec.daemon
        ),
        other => other.to_string(),
    };
    let old = fleet
        .status
        .as_ref()
        .map(|s| s.conditions.as_slice())
        .unwrap_or(&[]);
    let status = FleetStatus {
        observed_generation: fleet.metadata.generation,
        conditions: replace_condition(
            old,
            Cond::no("Ready", "Deleting", &message),
            fleet.metadata.generation,
            &ctx.k8s_now(),
        ),
    };
    if let Err(e) = patch_status(&ctx.client, fleet, &status).await {
        tracing::warn!(namespace = %fleet.namespace().unwrap_or_default(), name = %fleet.name_any(), "the Deleting condition was not written: {e}");
    }
}

async fn cleanup_fleet(fleet: Arc<Fleet>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = fleet.namespace().unwrap_or_default();
    let name = fleet.name_any();
    let key = Context::key(&namespace, &name);
    let selector = ListParams::default().labels(&format!("balerix.ai/fleet={name}"));

    // 1. every Agent gone (their finalizers harvest, §8.5)
    let agents: Api<Agent> = Api::namespaced(ctx.client.clone(), &namespace);
    let remaining = agents.list(&selector).await?.items;
    if !remaining.is_empty() {
        for agent in &remaining {
            if agent.metadata.deletion_timestamp.is_none() {
                agents
                    .delete(&agent.name_any(), &DeleteParams::default())
                    .await?;
            }
        }
        let names: Vec<String> = remaining.iter().map(ResourceExt::name_any).collect();
        return Err(Error::Waiting(format!(
            "Fleet {key}: {} agents still exist ({}); an Agent's harvest is skipped by the annotation {PURGE_ANNOTATION}=true on the Agent or the Fleet",
            remaining.len(),
            names.join(", ")
        )));
    }

    // 2. down on the Daemon; a Daemon that is gone has nothing to down
    if let Some(daemon) = daemon_of(&ctx, &namespace, &fleet.spec.daemon).await? {
        match client_for(&ctx, &daemon).await {
            Ok(client) => client.down(&name).await?,
            Err(Error::Missing(why)) => tracing::warn!(fleet = %key, "not downed: {why}"),
            Err(e) => return Err(e),
        }
    }

    // 3. `retain: None`: one cleanup Job per crew (§5.2)
    if fleet.spec.retain == Retain::None {
        let crews: Api<Crew> = Api::namespaced(ctx.client.clone(), &namespace);
        let job_ctx = JobContext {
            namespace: &namespace,
            daemon: &fleet.spec.daemon,
            images: &ctx.cfg.images,
            owner: owner_of(fleet.as_ref())?,
        };
        let mut pending = Vec::new();
        for crew in crews.list(&selector).await?.items {
            let ensured = ensure_job(
                ctx.as_ref(),
                fleet.as_ref(),
                remove_job(&job_ctx, &name, &crew.spec.crew)?,
                Some((&name, &crew.spec.crew)),
            )
            .await?;
            match ensured.outcome {
                JobOutcome::Succeeded(_) => {}
                // the namespace goes, and the crew's slice is not in it
                _ if ensured.namespace_terminating => {}
                JobOutcome::Failed(message) => pending.push(format!(
                    "{} (Job {} failed: {message})",
                    crew.spec.crew,
                    names::remove_job(&name, &crew.spec.crew)
                )),
                _ => pending.push(crew.spec.crew.clone()),
            }
        }
        if !pending.is_empty() {
            return Err(Error::Waiting(format!(
                "Fleet {key}: cleanup of crews {} not finished",
                pending.join(", ")
            )));
        }
    }

    ctx.records
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    ctx.fleet_agents
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    if let Some(list) = ctx
        .daemon_fleets
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&Context::key(&namespace, &fleet.spec.daemon))
    {
        list.retain(|f| f != &name);
    }
    tracing::info!(fleet = %key, "removed");
    Ok(Action::await_change())
}
