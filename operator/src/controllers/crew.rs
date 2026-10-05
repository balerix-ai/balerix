//! The Crew controller (Spec O §5.3, §21.2): the sync Job under the Job
//! rule, with the fleet pool Job's outcome read by name, into
//! `CacheReady` and `ToolsReady`.

use std::sync::Arc;

use futures_util::StreamExt;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Api, ResourceExt};

use super::fleet::daemon_of;
use super::jobs::{ensure_job, pods_of};
use super::{Context, Error, api_in, bounded, error_policy, patch_status, reconciled, report};
use crate::api::Crew;
use crate::desired::common::{JobOutcome, owner_of};
use crate::desired::jobs::{JobContext, crew_status, crew_sync_job, job_outcome};
use crate::desired::names;

pub async fn controller(ctx: Arc<Context>, watches: &kube::Client, namespace: Option<&str>) {
    let client = watches;
    Controller::new(
        api_in::<Crew>(client, namespace),
        watcher::Config::default(),
    )
    .owns(api_in::<Job>(client, namespace), watcher::Config::default())
    .run(bounded(reconcile), error_policy, ctx.clone())
    .for_each(|r| async move { report("crew", r) })
    .await;
}

/// `Ok` when the crew's cache and tools are ready; else why not.
pub fn crew_ready(crew: &Crew) -> Result<(), String> {
    let conditions = crew
        .status
        .as_ref()
        .map(|s| s.conditions.as_slice())
        .unwrap_or(&[]);
    for type_ in ["CacheReady", "ToolsReady"] {
        match conditions.iter().find(|c| c.type_ == type_) {
            Some(c) if c.status == "True" => {}
            Some(c) => {
                return Err(format!(
                    "{type_}: {}",
                    if c.message.is_empty() {
                        c.reason.clone()
                    } else {
                        c.message.clone()
                    }
                ));
            }
            None => return Err(format!("{type_}: not reported yet")),
        }
    }
    Ok(())
}

/// What the fleet's pool Job came to, read by name: it is the Fleet's.
async fn fleet_pool_outcome(
    ctx: &Context,
    namespace: &str,
    crew: &Crew,
) -> Result<JobOutcome, Error> {
    let name = names::fleet_pool_job(&crew.spec.fleet);
    let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), namespace);
    let Some(existing) = jobs.get_opt(&name).await? else {
        return Ok(JobOutcome::Absent);
    };
    let job_pods = pods_of(&pods, Some(&existing)).await?;
    // `job_outcome` compares the input hash with a wanted Job; the fleet
    // pool's wanted Job is the Fleet's to make, so the existing one is it
    Ok(job_outcome(Some(&existing), &job_pods, &existing))
}

pub async fn reconcile(crew: Arc<Crew>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = crew.namespace().unwrap_or_default();
    let name = crew.name_any();
    let Some(daemon) = daemon_of(&ctx, &namespace, &crew.spec.daemon).await? else {
        return Err(Error::Missing(format!(
            "Crew {namespace}/{name}: Daemon {} does not exist",
            crew.spec.daemon
        )));
    };
    let gh_secret = daemon
        .spec
        .credentials
        .github
        .as_ref()
        .map(|r| r.secret_name.clone());
    let job_ctx = JobContext {
        namespace: &namespace,
        daemon: &crew.spec.daemon,
        images: &ctx.cfg.images,
        owner: owner_of(crew.as_ref())?,
    };
    let wanted = crew_sync_job(&job_ctx, &crew.spec, gh_secret.as_deref())?;
    let sync = ensure_job(
        ctx.as_ref(),
        crew.as_ref(),
        wanted,
        Some((&crew.spec.fleet, &crew.spec.crew)),
    )
    .await?;
    let fleet_pool = fleet_pool_outcome(&ctx, &namespace, &crew).await?;
    let status = crew_status(&crew, &fleet_pool, &sync.outcome, &ctx.k8s_now());
    patch_status(&ctx.client, crew.as_ref(), &status).await?;
    reconciled(&ctx, crew.as_ref());
    Ok(Action::requeue(sync.again.min(ctx.run.period)))
}
