//! One rule for every Job the operator makes (Spec O §21.2): `backoffLimit:
//! 0`, a stale Job is replaced, a failed one is retried by the operator
//! after a doubling delay carried on its owner's `balerix.ai/attempts`
//! annotation, and a crew's sync, harvest and cleanup Jobs never run at
//! once (§5.3).

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Resource, ResourceExt};
use serde::de::DeserializeOwned;

use super::{Context, Error};
use crate::desired::common::JobOutcome;
use crate::desired::jobs::job_outcome;

/// On the owner: a JSON object from Job name to the attempt its next run
/// is. Absent or missing the name means attempt 1.
pub const ATTEMPTS_ANNOTATION: &str = "balerix.ai/attempts";

pub struct Ensured {
    pub outcome: JobOutcome,
    /// When to look again: soon while a Job runs or a lock holds, the
    /// rest of the delay for a failed one, the caller's period otherwise.
    pub again: Duration,
}

/// 30 s for attempt 1, doubling, at most 10 min (§21.2).
pub fn delay(attempt: u32) -> Duration {
    let n = attempt.max(1) - 1;
    Duration::from_secs((30u64 << n.min(5)).min(600))
}

fn attempts_of<K: Resource>(owner: &K) -> BTreeMap<String, u32> {
    owner
        .annotations()
        .get(ATTEMPTS_ANNOTATION)
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default()
}

/// When the Job failed: its `Failed` condition's transition, else its
/// creation. Unix seconds; `None` when the API server set neither.
fn failed_at(job: &Job) -> Option<i64> {
    let condition = job
        .status
        .as_ref()?
        .conditions
        .iter()
        .flatten()
        .find(|c| c.type_ == "Failed" && c.status == "True")
        .and_then(|c| c.last_transition_time.as_ref());
    condition
        .or(job.metadata.creation_timestamp.as_ref())
        .map(|t| t.0.as_second())
}

/// Whether a Job is still running: created and not yet succeeded or failed.
fn unfinished(job: &Job) -> bool {
    let status = job.status.as_ref();
    status.and_then(|s| s.succeeded).unwrap_or(0) == 0
        && status.and_then(|s| s.failed).unwrap_or(0) == 0
}

/// Whether another unfinished Job of this crew exists (§5.3's lock).
pub async fn crew_busy(
    ctx: &Context,
    namespace: &str,
    fleet: &str,
    crew: &str,
    except: &str,
) -> Result<bool, Error> {
    let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    let list = jobs
        .list(
            &ListParams::default()
                .labels(&format!("balerix.ai/fleet={fleet},balerix.ai/crew={crew}")),
        )
        .await?;
    Ok(list
        .items
        .iter()
        .any(|j| j.name_any() != except && unfinished(j)))
}

async fn set_attempts<K>(
    ctx: &Context,
    owner: &K,
    attempts: &BTreeMap<String, u32>,
) -> Result<(), Error>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope>
        + Clone
        + std::fmt::Debug
        + DeserializeOwned,
    K::DynamicType: Default,
{
    let namespace = owner.namespace().unwrap_or_default();
    let api: Api<K> = Api::namespaced(ctx.client.clone(), &namespace);
    let value =
        serde_json::to_string(attempts).map_err(crate::desired::common::DesiredError::from)?;
    api.patch(
        &owner.name_any(),
        &PatchParams::default(),
        &Patch::Merge(
            serde_json::json!({ "metadata": { "annotations": { ATTEMPTS_ANNOTATION: value } } }),
        ),
    )
    .await?;
    Ok(())
}

/// `wanted` exists and is current, or is on its way: creates an absent
/// one, replaces a stale one, leaves a running one, retries a failed one
/// after its delay. `crew_lock` is the `(fleet, crew)` whose other Jobs
/// must be finished before this one starts.
pub async fn ensure_job<K>(
    ctx: &Context,
    owner: &K,
    wanted: Job,
    crew_lock: Option<(&str, &str)>,
) -> Result<Ensured, Error>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope>
        + Clone
        + std::fmt::Debug
        + DeserializeOwned,
    K::DynamicType: Default,
{
    let namespace = wanted.namespace().unwrap_or_default();
    let name = wanted.name_any();
    let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), &namespace);
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let existing = jobs.get_opt(&name).await?;
    // by the Job's uid, not `job-name`: a deleted Job's pods keep the name
    // until garbage collection, and their messages are not this Job's
    let job_pods = match existing.as_ref().and_then(|j| j.metadata.uid.as_deref()) {
        Some(uid) => {
            pods.list(
                &ListParams::default().labels(&format!("batch.kubernetes.io/controller-uid={uid}")),
            )
            .await?
            .items
        }
        None => Vec::new(),
    };
    let outcome = job_outcome(existing.as_ref(), &job_pods, &wanted);
    let soon = Duration::from_secs(5);
    let mut attempts = attempts_of(owner);
    match &outcome {
        JobOutcome::Absent => {
            if let Some((fleet, crew)) = crew_lock
                && crew_busy(ctx, &namespace, fleet, crew, &name).await?
            {
                tracing::debug!(job = %name, "waiting: another Job of the crew runs");
                return Ok(Ensured {
                    outcome,
                    again: soon,
                });
            }
            match jobs.create(&PostParams::default(), &wanted).await {
                Ok(_) => {
                    tracing::info!(job = %name, attempt = attempts.get(&name).copied().unwrap_or(1), "created")
                }
                // made by a reconcile that raced this one: it is running
                Err(kube::Error::Api(e)) if e.code == 409 => {}
                Err(e) => return Err(e.into()),
            }
            Ok(Ensured {
                outcome: JobOutcome::Running,
                again: soon,
            })
        }
        JobOutcome::Stale => {
            jobs.delete(&name, &DeleteParams::background()).await?;
            if attempts.remove(&name).is_some() {
                set_attempts(ctx, owner, &attempts).await?;
            }
            tracing::info!(job = %name, "stale: replaced");
            Ok(Ensured {
                outcome,
                again: Duration::from_secs(2),
            })
        }
        JobOutcome::Running => Ok(Ensured {
            outcome,
            again: soon,
        }),
        JobOutcome::Succeeded(_) => {
            if attempts.remove(&name).is_some() {
                set_attempts(ctx, owner, &attempts).await?;
            }
            Ok(Ensured {
                outcome,
                again: ctx.run.period,
            })
        }
        JobOutcome::Failed(message) => {
            let attempt = attempts.get(&name).copied().unwrap_or(1);
            let now = ctx.now();
            let due = failed_at(
                existing
                    .as_ref()
                    .ok_or_else(|| Error::Missing(format!("Job {name} vanished")))?,
            )
            .unwrap_or(now)
                + delay(attempt).as_secs() as i64;
            if now >= due {
                jobs.delete(&name, &DeleteParams::background()).await?;
                attempts.insert(name.clone(), attempt + 1);
                set_attempts(ctx, owner, &attempts).await?;
                tracing::warn!(job = %name, attempt = attempt + 1, "failed: {message}; retrying");
                return Ok(Ensured {
                    outcome,
                    again: Duration::from_secs(2),
                });
            }
            Ok(Ensured {
                outcome,
                again: Duration::from_secs((due - now).max(1) as u64),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_delay_doubles_from_thirty_seconds_to_a_ten_minute_cap() {
        assert_eq!(delay(0), Duration::from_secs(30));
        assert_eq!(delay(1), Duration::from_secs(30));
        assert_eq!(delay(2), Duration::from_secs(60));
        assert_eq!(delay(5), Duration::from_secs(480));
        assert_eq!(delay(6), Duration::from_secs(600));
        assert_eq!(delay(60), Duration::from_secs(600));
    }

    #[test]
    fn failed_at_prefers_the_failed_condition_over_creation() {
        let job: Job = serde_json::from_value(serde_json::json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": { "name": "j", "namespace": "ns", "creationTimestamp": "2026-10-03T00:00:00Z" },
            "status": { "failed": 1, "conditions": [{ "type": "Failed", "status": "True", "lastTransitionTime": "2026-10-03T00:10:00Z" }] }
        })).unwrap();
        assert_eq!(failed_at(&job), Some(1_790_986_200)); // 2026-10-03T00:10:00Z
        let mut no_condition = job.clone();
        no_condition.status.as_mut().unwrap().conditions = None;
        assert_eq!(failed_at(&no_condition), Some(1_790_985_600)); // creation, 2026-10-03T00:00:00Z
        assert!(unfinished(&Job::default()));
        assert!(!unfinished(&job));
    }

    #[test]
    fn attempts_are_read_from_the_owner_annotation_and_default_to_one() {
        let job: Job = serde_json::from_value(serde_json::json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": { "name": "o", "namespace": "ns", "annotations": { ATTEMPTS_ANNOTATION: "{\"f-c-sync\": 3}" } }
        })).unwrap();
        let attempts = attempts_of(&job);
        assert_eq!(attempts.get("f-c-sync"), Some(&3));
        assert_eq!(*attempts.get("other").unwrap_or(&1), 1);
        assert!(attempts_of(&Job::default()).is_empty());
    }
}
