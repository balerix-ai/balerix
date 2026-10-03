//! The Jobs that write the shared volume (Spec O §8.3, §8.4, O-6): each
//! mounts its crew's slice where the agent pod has it (§20.3), read-write
//! where it writes. `balerix-agent`'s `crew-sync`, `pool-sync` and
//! `harvest` are their commands; the outcome is the container's
//! termination message.

use std::collections::BTreeMap;

use balerix_api::{GitAuth, GitSettings};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use serde_json::{Value, json};

use super::common::{
    Cond, DesiredError, HASH_ANNOTATION, Images, JobOutcome, conditions, container_security, hash,
    labels, pod_security, typed,
};
use super::names;
use crate::api::{AgentSpec, Crew, CrewSpec, CrewStatus};

/// Where a Job's init container sees the whole shared claim.
pub const VOLUME: &str = "/balerix/volume";
/// Where the slice is mounted, in a Job and in the agent pod (§7.4).
pub const SHARED: &str = "/balerix/shared";
/// A Job's emptyDir: the gh config, the git profile, nono's home, `HOME`.
pub const SCRATCH: &str = "/balerix/scratch";

pub struct JobContext<'a> {
    pub namespace: &'a str,
    pub daemon: &'a str,
    pub images: &'a Images,
    /// The owner reference, as `common::owner_of` makes it.
    pub owner: Value,
}

/// One directory of the volume and where under `SHARED` the Job has it.
struct Slice {
    sub_path: String,
    at: &'static str,
    writable: bool,
}

fn slice(sub_path: String, at: &'static str, writable: bool) -> Slice {
    Slice {
        sub_path,
        at,
        writable,
    }
}

struct Parts {
    name: String,
    component: &'static str,
    extra_labels: Vec<(&'static str, String)>,
    args: Vec<String>,
    slices: Vec<Slice>,
    volumes: Vec<Value>,
    mounts: Vec<Value>,
}

fn tool_args(table: &BTreeMap<String, String>) -> Vec<String> {
    table
        .iter()
        .flat_map(|(name, version)| ["--tool".to_string(), format!("{name}={version}")])
        .collect()
}

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_string()).collect()
}

fn job(ctx: &JobContext<'_>, parts: Parts) -> Result<Job, DesiredError> {
    let extra: Vec<(&str, &str)> = parts
        .extra_labels
        .iter()
        .map(|(k, v)| (*k, v.as_str()))
        .collect();
    let labels = labels(ctx.daemon, parts.component, &extra);
    // The kubelet makes a missing sub-path owned by root; made here as
    // the Job's own user, the Job can write it.
    let mut mkdir = strings(&["mkdir", "-p"]);
    mkdir.extend(
        parts
            .slices
            .iter()
            .map(|s| format!("{VOLUME}/{}", s.sub_path)),
    );
    let mut mounts: Vec<Value> = parts
        .slices
        .iter()
        .map(|s| {
            json!({
                "name": "shared",
                "mountPath": format!("{SHARED}/{}", s.at),
                "subPath": s.sub_path,
                "readOnly": !s.writable,
            })
        })
        .collect();
    mounts.push(json!({ "name": "scratch", "mountPath": SCRATCH }));
    mounts.extend(parts.mounts);
    let mut volumes = vec![
        json!({ "name": "shared", "persistentVolumeClaim": { "claimName": names::shared_claim(ctx.daemon) } }),
        json!({ "name": "scratch", "emptyDir": {} }),
    ];
    volumes.extend(parts.volumes);
    let input = hash(&json!({
        "image": ctx.images.agent,
        "args": parts.args,
        "mounts": mounts,
        "volumes": volumes,
    }));
    typed(json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "name": parts.name,
            "namespace": ctx.namespace,
            "labels": labels,
            "annotations": { HASH_ANNOTATION: input },
            "ownerReferences": [ctx.owner],
        },
        "spec": {
            // the operator retries with back-off (§8.3); the Job does not
            "backoffLimit": 0,
            "template": {
                "metadata": { "labels": labels },
                "spec": {
                    "restartPolicy": "Never",
                    "automountServiceAccountToken": false,
                    "enableServiceLinks": false,
                    "securityContext": pod_security(),
                    "initContainers": [{
                        "name": "slice",
                        "image": ctx.images.agent,
                        "command": mkdir,
                        "securityContext": container_security(),
                        "volumeMounts": [{ "name": "shared", "mountPath": VOLUME }],
                    }],
                    "containers": [{
                        "name": "job",
                        "image": ctx.images.agent,
                        "command": ["balerix-agent"],
                        "args": parts.args,
                        "env": [{ "name": "HOME", "value": SCRATCH }],
                        "securityContext": container_security(),
                        "volumeMounts": mounts,
                    }],
                    "volumes": volumes,
                },
            },
        },
    }))
}

/// `pools/daemon`: the system table this image embeds (§20.3).
pub fn daemon_pool_job(ctx: &JobContext<'_>) -> Result<Job, DesiredError> {
    job(
        ctx,
        Parts {
            name: names::daemon_pool_job(ctx.daemon),
            component: "pool",
            extra_labels: vec![],
            args: strings(&["pool-sync", "--level", "daemon"]),
            slices: vec![slice(names::vol_daemon_pool(), "daemon", true)],
            volumes: vec![],
            mounts: vec![],
        },
    )
}

/// The fleet's `defaults.tools`, once per fleet and before its crews'
/// Jobs: two crews' Jobs would otherwise write one pool at once (§20.3).
pub fn fleet_pool_job(
    ctx: &JobContext<'_>,
    fleet: &str,
    table: &BTreeMap<String, String>,
) -> Result<Job, DesiredError> {
    let mut args = strings(&["pool-sync", "--level", "fleet", "--fleet", fleet]);
    args.extend(tool_args(table));
    job(
        ctx,
        Parts {
            name: names::fleet_pool_job(fleet),
            component: "pool",
            extra_labels: vec![("balerix.ai/fleet", fleet.to_string())],
            args,
            slices: vec![
                slice(names::vol_fleet_pool(fleet), "fleet", true),
                slice(names::vol_daemon_pool(), "daemon", false),
            ],
            volumes: vec![],
            mounts: vec![],
        },
    )
}

/// The crew cache and the crew pool. `gh_secret` is the Daemon's
/// `credentials.github.secretName`; a crew whose `git.auth` is `gh`
/// cannot sync without it.
pub fn crew_sync_job(
    ctx: &JobContext<'_>,
    crew: &CrewSpec,
    gh_secret: Option<&str>,
) -> Result<Job, DesiredError> {
    let git: GitSettings = typed(crew.git.clone())?;
    let mut args = strings(&["crew-sync", "--crew"]);
    args.push(format!("{}/{}", crew.fleet, crew.crew));
    args.extend(["--repo".to_string(), crew.repo.clone()]);
    args.extend(["--ref".to_string(), crew.git_ref.clone()]);
    let (mut volumes, mut mounts) = (vec![], vec![]);
    if git.auth == GitAuth::Gh {
        let secret = gh_secret.ok_or(DesiredError::Missing(
            "the Daemon",
            "spec.credentials.github (the crew's git.auth is gh)",
        ))?;
        args.extend(strings(&["--gh-token-file", "/balerix/secret/gh-token"]));
        // 0440: the pod's fsGroup reads it; nothing else does
        volumes.push(json!({
            "name": "gh",
            "secret": {
                "secretName": secret,
                "items": [{ "key": "token", "path": "gh-token" }],
                "defaultMode": 0o440,
            },
        }));
        mounts.push(json!({ "name": "gh", "mountPath": "/balerix/secret", "readOnly": true }));
    }
    args.extend(tool_args(&crew.tools));
    job(
        ctx,
        Parts {
            name: names::sync_job(&crew.fleet, &crew.crew),
            component: "sync",
            extra_labels: vec![
                ("balerix.ai/fleet", crew.fleet.clone()),
                ("balerix.ai/crew", crew.crew.clone()),
            ],
            args,
            slices: vec![
                slice(names::vol_crew_repo(&crew.fleet, &crew.crew), "repo", true),
                slice(names::vol_crew_pool(&crew.fleet, &crew.crew), "crew", true),
                slice(names::vol_fleet_pool(&crew.fleet), "fleet", false),
                slice(names::vol_daemon_pool(), "daemon", false),
            ],
            volumes,
            mounts,
        },
    )
}

/// The agent's branch into the crew cache, after its pod is gone: the
/// claim read-only, the cache read-write (§8.4). It needs Landlock, like
/// any agent.
pub fn harvest_job(
    ctx: &JobContext<'_>,
    agent_name: &str,
    agent: &AgentSpec,
) -> Result<Job, DesiredError> {
    let mut args = strings(&["harvest", "--agent"]);
    args.push(format!("{}/{}/{}", agent.fleet, agent.crew, agent.agent));
    job(
        ctx,
        Parts {
            name: names::harvest_job(agent_name),
            component: "harvest",
            extra_labels: vec![
                ("balerix.ai/fleet", agent.fleet.clone()),
                ("balerix.ai/crew", agent.crew.clone()),
                ("balerix.ai/agent", agent.agent.clone()),
            ],
            args,
            slices: vec![
                slice(
                    names::vol_crew_repo(&agent.fleet, &agent.crew),
                    "repo",
                    true,
                ),
                // the crew's logs and its `no-hooks` directory
                slice(
                    names::vol_crew_pool(&agent.fleet, &agent.crew),
                    "crew",
                    true,
                ),
            ],
            volumes: vec![json!({
                "name": "agent",
                "persistentVolumeClaim": { "claimName": agent_name, "readOnly": true },
            })],
            mounts: vec![
                json!({ "name": "agent", "mountPath": "/balerix/agent", "readOnly": true }),
            ],
        },
    )
}

fn input_hash(job: &Job) -> Option<&String> {
    job.metadata.annotations.as_ref()?.get(HASH_ANNOTATION)
}

/// The last termination message any of the Job's containers left.
fn message(pods: &[Pod]) -> Option<String> {
    pods.iter().rev().find_map(|pod| {
        let status = pod.status.as_ref()?;
        status
            .container_statuses
            .iter()
            .flatten()
            .chain(status.init_container_statuses.iter().flatten())
            .find_map(|c| c.state.as_ref()?.terminated.as_ref()?.message.clone())
            .map(|m| m.trim_end().to_string())
    })
}

/// `existing` is the Job by `wanted`'s name, if there is one; `pods` are
/// that Job's pods.
pub fn job_outcome(existing: Option<&Job>, pods: &[Pod], wanted: &Job) -> JobOutcome {
    let Some(job) = existing else {
        return JobOutcome::Absent;
    };
    if input_hash(job) != input_hash(wanted) {
        return JobOutcome::Stale;
    }
    let status = job.status.as_ref();
    if status.and_then(|s| s.succeeded).unwrap_or(0) >= 1 {
        // a success whose pod is gone has no message to report; that is not a failure
        return JobOutcome::Succeeded(message(pods).unwrap_or_default());
    }
    if status.and_then(|s| s.failed).unwrap_or(0) >= 1 {
        return JobOutcome::Failed(
            message(pods).unwrap_or_else(|| "the job failed and left no message".to_string()),
        );
    }
    JobOutcome::Running
}

/// `CacheReady` and `ToolsReady` (§4.3) from the fleet's pool Job and the
/// crew's sync Job. A sync that failed says which half in its message's
/// prefix (`cache:` or `tools:`); the tools half runs after the cache, so
/// a `tools:` failure means the cache is synced. `cacheRef` is the commit
/// the last successful sync reported, and stays until the next one.
pub fn crew_status(
    crew: &Crew,
    fleet_pool: &JobOutcome,
    sync: &JobOutcome,
    now: &Time,
) -> CrewStatus {
    let syncing = || Cond::no("CacheReady", "Syncing", "");
    let (cache, tools, commit) = match (fleet_pool, sync) {
        (JobOutcome::Failed(message), _) => (
            syncing(),
            Cond::no("ToolsReady", "FleetPoolFailed", message),
            None,
        ),
        (JobOutcome::Succeeded(_), JobOutcome::Succeeded(commit)) => (
            Cond::yes("CacheReady", "Synced", ""),
            Cond::yes("ToolsReady", "Synced", ""),
            Some(commit.clone()),
        ),
        (JobOutcome::Succeeded(_), JobOutcome::Failed(message))
            if message.starts_with("tools:") =>
        {
            (
                Cond::yes("CacheReady", "Synced", ""),
                Cond::no("ToolsReady", "SyncFailed", message),
                None,
            )
        }
        (JobOutcome::Succeeded(_), JobOutcome::Failed(message)) => (
            Cond::no("CacheReady", "SyncFailed", message),
            Cond::unknown("ToolsReady", "SyncFailed", ""),
            None,
        ),
        _ => (syncing(), Cond::no("ToolsReady", "Syncing", ""), None),
    };
    let old = crew.status.as_ref();
    CrewStatus {
        observed_generation: crew.metadata.generation,
        conditions: conditions(
            // cannot fire: no old status means no old conditions
            old.map(|s| s.conditions.as_slice()).unwrap_or_default(),
            &[cache, tools],
            crew.metadata.generation,
            now,
        ),
        // a sync that reported no commit (its pod gone) keeps the old one
        cache_ref: commit
            .filter(|c| !c.is_empty())
            .or_else(|| old.and_then(|s| s.cache_ref.clone())),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::api::{AgentSpec, Crew, CrewSpec, CrewStatus};

    fn images() -> Images {
        Images::for_version("0.2.0")
    }

    fn ctx(images: &Images) -> JobContext<'_> {
        JobContext {
            namespace: "team-a",
            daemon: "default",
            images,
            owner: json!({
                "apiVersion": "balerix.ai/v1alpha1", "kind": "Crew", "name": "payments-backend",
                "uid": "crew-uid", "controller": true
            }),
        }
    }

    fn crew(auth: &str) -> CrewSpec {
        CrewSpec {
            daemon: "default".into(),
            fleet: "payments".into(),
            crew: "backend".into(),
            repo: "acme/payments-api".into(),
            git_ref: "main".into(),
            git: json!({ "push": true, "auth": auth }),
            fleet_tools: BTreeMap::from([("node".into(), "22.11.0".into())]),
            tools: BTreeMap::from([("python".into(), "3.12.8".into())]),
        }
    }

    fn agent() -> AgentSpec {
        AgentSpec {
            daemon: "default".into(),
            fleet: "payments".into(),
            crew: "backend".into(),
            agent: "alice".into(),
            repo: "acme/payments-api".into(),
            git_ref: "main".into(),
            git: json!({}),
            settings: json!({}),
            spec_hash: "h".into(),
        }
    }

    #[test]
    fn the_four_jobs() {
        let images = images();
        let c = ctx(&images);
        insta::assert_yaml_snapshot!("job_daemon_pool", daemon_pool_job(&c).unwrap());
        insta::assert_yaml_snapshot!(
            "job_fleet_pool",
            fleet_pool_job(&c, "payments", &crew("gh").fleet_tools).unwrap()
        );
        insta::assert_yaml_snapshot!(
            "job_crew_sync",
            crew_sync_job(&c, &crew("gh"), Some("gh-token")).unwrap()
        );
        insta::assert_yaml_snapshot!(
            "job_harvest",
            harvest_job(&c, "payments-backend-alice", &agent()).unwrap()
        );
    }

    #[test]
    fn a_public_crew_mounts_no_token_and_a_gh_crew_needs_the_daemons_secret() {
        let images = images();
        let c = ctx(&images);
        let public = serde_json::to_value(crew_sync_job(&c, &crew("none"), None).unwrap()).unwrap();
        let text = public.to_string();
        assert!(!text.contains("gh-token"), "{text}");
        let e = crew_sync_job(&c, &crew("gh"), None)
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            "the Daemon has no spec.credentials.github (the crew's git.auth is gh)"
        );
    }

    #[test]
    fn a_jobs_hash_follows_its_input() {
        let images = images();
        let c = ctx(&images);
        let hash_of =
            |job: &Job| job.metadata.annotations.as_ref().unwrap()[HASH_ANNOTATION].clone();
        let a = crew_sync_job(&c, &crew("gh"), Some("gh-token")).unwrap();
        let same = crew_sync_job(&c, &crew("gh"), Some("gh-token")).unwrap();
        assert_eq!(hash_of(&a), hash_of(&same));
        let mut moved = crew("gh");
        moved.git_ref = "release".into();
        assert_ne!(
            hash_of(&a),
            hash_of(&crew_sync_job(&c, &moved, Some("gh-token")).unwrap())
        );
        let newer = Images::for_version("0.3.0");
        assert_ne!(
            hash_of(&a),
            hash_of(&crew_sync_job(&ctx(&newer), &crew("gh"), Some("gh-token")).unwrap())
        );
    }

    fn observed(job: &Job, status: serde_json::Value) -> Job {
        let mut v = serde_json::to_value(job).unwrap();
        v["status"] = status;
        serde_json::from_value(v).unwrap()
    }

    fn pod(statuses: serde_json::Value) -> Pod {
        serde_json::from_value(json!({ "metadata": { "name": "p" }, "status": statuses })).unwrap()
    }

    #[test]
    fn an_outcome_is_read_from_the_job_and_its_pods_termination_message() {
        let images = images();
        let c = ctx(&images);
        let wanted = daemon_pool_job(&c).unwrap();
        assert_eq!(job_outcome(None, &[], &wanted), JobOutcome::Absent);
        assert_eq!(
            job_outcome(Some(&wanted), &[], &wanted),
            JobOutcome::Running
        );

        let done = observed(&wanted, json!({ "succeeded": 1 }));
        let said = pod(json!({ "containerStatuses": [{
            "name": "job", "ready": false, "restartCount": 0, "image": "i", "imageID": "",
            "state": { "terminated": { "exitCode": 0, "message": "synced\n" } }
        }] }));
        assert_eq!(
            job_outcome(Some(&done), &[said], &wanted),
            JobOutcome::Succeeded("synced".into())
        );

        let failed = observed(&wanted, json!({ "failed": 1 }));
        let why = pod(json!({ "containerStatuses": [{
            "name": "job", "ready": false, "restartCount": 0, "image": "i", "imageID": "",
            "state": { "terminated": { "exitCode": 1, "message": "tools: system: mise install failed\n" } }
        }] }));
        assert_eq!(
            job_outcome(Some(&failed), &[why], &wanted),
            JobOutcome::Failed("tools: system: mise install failed".into())
        );
        // the init container could not make the directories
        let init = pod(json!({ "initContainerStatuses": [{
            "name": "slice", "ready": false, "restartCount": 0, "image": "i", "imageID": "",
            "state": { "terminated": { "exitCode": 1, "reason": "Error" } }
        }] }));
        assert_eq!(
            job_outcome(Some(&failed), &[init], &wanted),
            JobOutcome::Failed("the job failed and left no message".into())
        );

        let newer = Images::for_version("0.3.0");
        let other = daemon_pool_job(&ctx(&newer)).unwrap();
        assert_eq!(job_outcome(Some(&done), &[], &other), JobOutcome::Stale);
    }

    fn crew_object(cache_ref: Option<&str>) -> Crew {
        let mut c = Crew::new("payments-backend", crew("gh"));
        c.metadata.generation = Some(5);
        c.status = cache_ref.map(|r| CrewStatus {
            cache_ref: Some(r.to_string()),
            ..Default::default()
        });
        c
    }

    fn crew_cond<'a>(status: &'a CrewStatus, type_: &str) -> (&'a str, &'a str, &'a str) {
        let c = status.conditions.iter().find(|c| c.type_ == type_).unwrap();
        (c.status.as_str(), c.reason.as_str(), c.message.as_str())
    }

    /// §5.3, §20.3: the sync Job's message says which half failed.
    #[test]
    fn a_crews_conditions_follow_the_two_jobs_and_the_messages_prefix() {
        let now = Time(k8s_openapi::jiff::Timestamp::from_second(1_800_000_000).unwrap());
        let ok = JobOutcome::Succeeded("synced".into());
        let s = crew_status(
            &crew_object(None),
            &ok,
            &JobOutcome::Succeeded("abc123".into()),
            &now,
        );
        assert_eq!(crew_cond(&s, "CacheReady"), ("True", "Synced", ""));
        assert_eq!(crew_cond(&s, "ToolsReady"), ("True", "Synced", ""));
        assert_eq!(s.cache_ref.as_deref(), Some("abc123"));
        assert_eq!(s.observed_generation, Some(5));

        let s = crew_status(&crew_object(Some("old")), &ok, &JobOutcome::Running, &now);
        assert_eq!(crew_cond(&s, "CacheReady"), ("False", "Syncing", ""));
        assert_eq!(crew_cond(&s, "ToolsReady"), ("False", "Syncing", ""));
        assert_eq!(
            s.cache_ref.as_deref(),
            Some("old"),
            "the last commit fetched stays"
        );

        let cache =
            JobOutcome::Failed("cache: payments/backend: the remote has no branch nope".into());
        let s = crew_status(&crew_object(Some("old")), &ok, &cache, &now);
        assert_eq!(
            crew_cond(&s, "CacheReady"),
            (
                "False",
                "SyncFailed",
                "cache: payments/backend: the remote has no branch nope"
            )
        );
        assert_eq!(crew_cond(&s, "ToolsReady"), ("Unknown", "SyncFailed", ""));

        let tools = JobOutcome::Failed(
            "tools: payments/backend: crew payments/backend: mise install failed".into(),
        );
        let s = crew_status(&crew_object(None), &ok, &tools, &now);
        assert_eq!(crew_cond(&s, "CacheReady"), ("True", "Synced", ""));
        assert_eq!(crew_cond(&s, "ToolsReady").0, "False");
        assert_eq!(crew_cond(&s, "ToolsReady").1, "SyncFailed");

        // the fleet's pool comes first; the crew's Job has not run
        let s = crew_status(
            &crew_object(None),
            &JobOutcome::Failed("tools: payments: fleet payments: boom".into()),
            &JobOutcome::Absent,
            &now,
        );
        assert_eq!(
            crew_cond(&s, "ToolsReady"),
            (
                "False",
                "FleetPoolFailed",
                "tools: payments: fleet payments: boom"
            )
        );
        assert_eq!(crew_cond(&s, "CacheReady"), ("False", "Syncing", ""));
    }
}
