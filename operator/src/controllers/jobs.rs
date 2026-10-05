//! One rule for every Job the operator makes (Spec O §21.2): `backoffLimit:
//! 0`, a stale Job is replaced, a failed one is retried by the operator
//! after a doubling delay carried on its owner's `balerix.ai/attempts`
//! annotation, and a crew's sync, harvest and cleanup Jobs never run at
//! once (§5.3): the crew's lock in the `Context` is held across the
//! check and the create, by a task that outlives a dropped reconcile, and a stale Job is deleted in the foreground,
//! so it stays listed, and keeps the crew busy, until its pods are gone.

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Client, Resource, ResourceExt};
use serde::de::DeserializeOwned;

use super::{Context, Error};
use crate::desired::common::JobOutcome;
use crate::desired::jobs::{job_failed, job_outcome};

/// On the owner: a JSON object from Job name to the attempt its next run
/// is. Absent or missing the name means attempt 1.
pub const ATTEMPTS_ANNOTATION: &str = "balerix.ai/attempts";

pub struct Ensured {
    pub outcome: JobOutcome,
    /// When to look again: soon while a Job runs or a lock holds, the
    /// rest of the delay for a failed one, the caller's period otherwise.
    pub again: Duration,
    /// The API server refused the create because the namespace is being
    /// deleted: no Job will ever run there, and the namespace takes
    /// whatever the Job would have cleaned up.
    pub namespace_terminating: bool,
}

impl Ensured {
    fn new(outcome: JobOutcome, again: Duration) -> Self {
        Self {
            outcome,
            again,
            namespace_terminating: false,
        }
    }
}

/// A create refused by the `NamespaceLifecycle` admission: the namespace
/// has a deletion timestamp.
fn namespace_terminating(status: &kube::core::Status) -> bool {
    status.code == 403
        && status
            .details
            .as_ref()
            .is_some_and(|d| d.causes.iter().any(|c| c.reason == "NamespaceTerminating"))
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
    job.status.as_ref().and_then(|s| s.succeeded).unwrap_or(0) == 0 && !job_failed(job)
}

/// What holds a crew (§5.3): one of its Jobs unfinished or being deleted,
/// or one of its Jobs' pods not yet finished or being deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// `Job` or `Pod`.
    pub kind: &'static str,
    pub name: String,
    /// Unix seconds of its deletion timestamp, while it is being deleted.
    pub deleting_since: Option<i64>,
}

fn deleting_since(meta: &ObjectMeta) -> Option<i64> {
    meta.deletion_timestamp.as_ref().map(|t| t.0.as_second())
}

/// What holds the crew, other than the Job `except`. A stale Job deleted
/// in the foreground is listed until its pods are gone. A pod outlives its
/// Job when the Job is collected in the background, as a dropped crew's
/// sync is (§22.3); `job-name` keeps the agents' own pods out.
pub async fn crew_holders(
    client: &Client,
    namespace: &str,
    fleet: &str,
    crew: &str,
    except: &str,
) -> Result<Vec<Holder>, Error> {
    let selector = format!("balerix.ai/fleet={fleet},balerix.ai/crew={crew}");
    let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);
    let mut holders: Vec<Holder> = jobs
        .list(&ListParams::default().labels(&selector))
        .await?
        .items
        .iter()
        .filter(|j| {
            j.name_any() != except && (unfinished(j) || j.metadata.deletion_timestamp.is_some())
        })
        .map(|j| Holder {
            kind: "Job",
            name: j.name_any(),
            deleting_since: deleting_since(&j.metadata),
        })
        .collect();
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let finished = |p: &Pod| {
        matches!(
            p.status.as_ref().and_then(|s| s.phase.as_deref()),
            Some("Succeeded" | "Failed")
        )
    };
    holders.extend(
        pods.list(
            &ListParams::default().labels(&format!("{selector},batch.kubernetes.io/job-name")),
        )
        .await?
        .items
        .iter()
        .filter(|p| p.metadata.deletion_timestamp.is_some() || !finished(p))
        .map(|p| Holder {
            kind: "Pod",
            name: p.name_any(),
            deleting_since: deleting_since(&p.metadata),
        }),
    );
    Ok(holders)
}

/// The holder deleting longest, once that is `after` or more: a pod on a
/// lost node never finishes its deletion (§22.3).
fn stuck(holders: &[Holder], now: i64, after: Duration) -> Option<&Holder> {
    holders
        .iter()
        .filter(|h| {
            h.deleting_since
                .is_some_and(|t| now - t >= after.as_secs() as i64)
        })
        .min_by_key(|h| h.deleting_since)
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

/// A Job's pods, by the Job's uid and not `job-name`: a deleted Job's
/// pods keep the name until garbage collection, and their messages are
/// not this Job's.
pub async fn pods_of(pods: &Api<Pod>, job: Option<&Job>) -> Result<Vec<Pod>, Error> {
    match job.and_then(|j| j.metadata.uid.as_deref()) {
        Some(uid) => Ok(pods
            .list(
                &ListParams::default().labels(&format!("batch.kubernetes.io/controller-uid={uid}")),
            )
            .await?
            .items),
        None => Ok(Vec::new()),
    }
}

/// What the busy check and the create came to.
enum Created {
    Made,
    /// Made by a reconcile that raced this one: it is running.
    Raced,
    Busy(Vec<Holder>),
    NamespaceTerminating,
}

/// The crew's busy check and the create, under the crew's lock when
/// `crew` is given. Owns everything, so it can run in a task of its own.
async fn check_and_create(
    client: Client,
    namespace: String,
    crew: Option<(String, String)>,
    wanted: Job,
) -> Result<Created, Error> {
    if let Some((fleet, crew)) = &crew {
        let holders = crew_holders(&client, &namespace, fleet, crew, &wanted.name_any()).await?;
        if !holders.is_empty() {
            return Ok(Created::Busy(holders));
        }
    }
    let jobs: Api<Job> = Api::namespaced(client, &namespace);
    match jobs.create(&PostParams::default(), &wanted).await {
        Ok(_) => Ok(Created::Made),
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(Created::Raced),
        Err(kube::Error::Api(e)) if namespace_terminating(&e) => Ok(Created::NamespaceTerminating),
        Err(e) => Err(e.into()),
    }
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
    let soon = Duration::from_secs(5);
    // a Job on its way out (a foreground delete waits for its pods) is
    // neither the wanted one nor a failure to count: wait for it to go
    if existing
        .as_ref()
        .is_some_and(|j| j.metadata.deletion_timestamp.is_some())
    {
        tracing::debug!(job = %name, "waiting: the previous Job is being deleted");
        return Ok(Ensured::new(JobOutcome::Running, Duration::from_secs(2)));
    }
    let job_pods = pods_of(&pods, existing.as_ref()).await?;
    let outcome = job_outcome(existing.as_ref(), &job_pods, &wanted);
    let mut attempts = attempts_of(owner);
    match &outcome {
        JobOutcome::Absent => {
            // the crew's lock across the check and the create (§5.3), kept
            // by a task of its own until the create is answered (§22.3)
            let work = check_and_create(
                ctx.client.clone(),
                namespace.clone(),
                crew_lock.map(|(fleet, crew)| (fleet.to_string(), crew.to_string())),
                wanted,
            );
            let created = match crew_lock {
                Some((fleet, crew)) => {
                    ctx.lock_crew(&namespace, fleet, crew)
                        .await
                        .hold_through(work)
                        .await??
                }
                None => work.await?,
            };
            match created {
                Created::Busy(holders) => {
                    tracing::debug!(job = %name, ?holders, "waiting: the crew is held");
                    if let (Some((fleet, crew)), Some(h)) =
                        (crew_lock, stuck(&holders, ctx.now(), ctx.run.stuck_after))
                    {
                        let since = h
                            .deleting_since
                            .and_then(|t| k8s_openapi::jiff::Timestamp::from_second(t).ok())
                            .map(|t| t.to_string())
                            .unwrap_or_default();
                        let note = format!(
                            "crew {fleet}/{crew} waits on {} {}, being deleted since {since}: the crew stays locked until it is gone",
                            h.kind, h.name
                        );
                        ctx.warn(owner, "CrewLocked", note).await;
                    }
                    Ok(Ensured::new(outcome, soon))
                }
                Created::NamespaceTerminating => {
                    tracing::warn!(namespace = %namespace, job = %name, "not created: the namespace is being deleted");
                    Ok(Ensured {
                        namespace_terminating: true,
                        ..Ensured::new(outcome, ctx.run.period)
                    })
                }
                Created::Made => {
                    tracing::info!(job = %name, attempt = attempts.get(&name).copied().unwrap_or(1), "created");
                    Ok(Ensured::new(JobOutcome::Running, soon))
                }
                Created::Raced => Ok(Ensured::new(JobOutcome::Running, soon)),
            }
        }
        JobOutcome::Stale => {
            // foreground: the Job stays, deleting, until its pods are gone,
            // so a running predecessor keeps the crew busy (§5.3)
            jobs.delete(&name, &DeleteParams::foreground()).await?;
            if attempts.remove(&name).is_some() {
                set_attempts(ctx, owner, &attempts).await?;
            }
            tracing::info!(job = %name, "stale: replaced");
            Ok(Ensured::new(outcome, Duration::from_secs(2)))
        }
        JobOutcome::Running => Ok(Ensured::new(outcome, soon)),
        JobOutcome::Succeeded(_) => {
            if attempts.remove(&name).is_some() {
                set_attempts(ctx, owner, &attempts).await?;
            }
            Ok(Ensured::new(outcome, ctx.run.period))
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
                return Ok(Ensured::new(outcome, Duration::from_secs(2)));
            }
            Ok(Ensured::new(
                outcome,
                Duration::from_secs((due - now).max(1) as u64),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_stuck_holder_is_the_one_deleting_longest_past_the_bound() {
        let holder = |kind, name: &str, since| Holder {
            kind,
            name: name.to_string(),
            deleting_since: since,
        };
        let after = Duration::from_secs(300);
        let holders = vec![
            holder("Job", "f-c-sync", None),
            holder("Pod", "young", Some(1_000)),
            holder("Pod", "old", Some(500)),
        ];
        assert_eq!(stuck(&holders, 1_200, after), Some(&holders[2]));
        // nothing has been deleting for 300 s yet
        assert_eq!(stuck(&holders, 700, after), None);
        assert_eq!(stuck(&holders[..1], 10_000, after), None);
    }

    #[test]
    fn a_job_failed_by_its_condition_alone_is_finished() {
        let job: Job = serde_json::from_value(serde_json::json!({
            "apiVersion": "batch/v1", "kind": "Job", "metadata": { "name": "j" },
            "status": { "conditions": [{ "type": "Failed", "status": "True", "reason": "DeadlineExceeded" }] }
        })).unwrap();
        assert!(!unfinished(&job));
    }

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
