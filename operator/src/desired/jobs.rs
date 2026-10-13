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
/// The directory of an agent claim that is mounted at `/balerix/agent`.
/// A volume's root belongs to root, and the sidecar narrows the agent
/// root to 0700 (`write_home`), which only its owner may do: the agent
/// pod's `claim` init container makes this directory as uid 10001.
pub const AGENT_DIR: &str = "agent";
/// Every Job's `activeDeadlineSeconds`: a pod that never starts (an image
/// that cannot be pulled, a claim that is gone) fails the Job, with the
/// Job's own message, under the Job rule, instead of leaving it running
/// forever. An hour, for a slow pool install.
pub const JOB_DEADLINE_SECONDS: i64 = 3600;

/// An unowned Job's `ttlSecondsAfterFinished`: the TTL controller deletes
/// it, and its pods, an hour after it finishes. Not the finalizer: a pass
/// from a cache that still shows the finalizer would find no Job and make
/// it again, with nothing left to collect it. An hour outlasts any such
/// pass, and `activeDeadlineSeconds` bounds how long it takes to finish.
pub const FINISHED_TTL_SECONDS: i64 = 3600;

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
    /// Whether the Job carries `ctx.owner`. A harvest or a cleanup runs
    /// while its owner is deleted, and under a foreground deletion the
    /// garbage collector would delete a Job owned by it before it ran:
    /// those two carry none, and go `FINISHED_TTL_SECONDS` after they
    /// finish (§26.3).
    owned: bool,
    component: &'static str,
    extra_labels: Vec<(&'static str, String)>,
    args: Vec<String>,
    slices: Vec<Slice>,
    volumes: Vec<Value>,
    mounts: Vec<Value>,
    /// More of the input hash, beyond the pod's own shape: a branch
    /// change's old branch (§27.2). `Null` adds nothing, so the other
    /// Jobs' hashes are what they were.
    also_hashed: Value,
    /// A directory under the agent claim's `AGENT_DIR` the `slice` init
    /// container makes as the Job's user, with that sub-path (never the
    /// whole claim) at `CLAIM_INIT`: a sub-path the kubelet made would be
    /// root's.
    claim_dir: Option<String>,
}

/// Where a branch change's `slice` init container sees the claim's
/// `AGENT_DIR`.
const CLAIM_INIT: &str = "/balerix/claim";

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
    let mut init_mounts = vec![json!({ "name": "shared", "mountPath": VOLUME })];
    if let Some(dir) = &parts.claim_dir {
        mkdir.push(format!("{CLAIM_INIT}/{dir}"));
        init_mounts.push(json!({ "name": "agent", "mountPath": CLAIM_INIT, "subPath": AGENT_DIR }));
    }
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
    mounts.push(json!({ "name": "tmp", "mountPath": "/tmp" }));
    mounts.extend(parts.mounts);
    let mut volumes = vec![
        json!({ "name": "shared", "persistentVolumeClaim": { "claimName": names::shared_claim(ctx.daemon) } }),
        json!({ "name": "scratch", "emptyDir": {} }),
        json!({ "name": "tmp", "emptyDir": {} }),
    ];
    volumes.extend(parts.volumes);
    let mut input = json!({
        "image": ctx.images.agent,
        "args": parts.args,
        "mounts": mounts,
        "volumes": volumes,
    });
    // an unowned Job outlives its owner by the TTL: one made for an
    // earlier object of the same name is stale, not this one's outcome
    if !parts.owned {
        input["for"] = ctx.owner["uid"].clone();
    }
    if !parts.also_hashed.is_null() {
        input["also"] = parts.also_hashed;
    }
    let input = hash(&input);
    let mut metadata = json!({
        "name": parts.name,
        "namespace": ctx.namespace,
        "labels": labels,
        "annotations": { HASH_ANNOTATION: input },
    });
    if parts.owned {
        metadata["ownerReferences"] = json!([ctx.owner]);
    }
    let mut job = json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": metadata,
        "spec": {
            // the operator retries with back-off (§8.3); the Job does not
            "backoffLimit": 0,
            "activeDeadlineSeconds": JOB_DEADLINE_SECONDS,
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
                        // a failed `mkdir` reports its stderr as the message
                        "terminationMessagePolicy": "FallbackToLogsOnError",
                        "securityContext": container_security(),
                        "volumeMounts": init_mounts,
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
    });
    if !parts.owned {
        job["spec"]["ttlSecondsAfterFinished"] = json!(FINISHED_TTL_SECONDS);
    }
    typed(job)
}

/// The cleanup Job (§21.2): `crew-remove` over the crew's cache and pool,
/// read-write, after the Fleet's Agents are gone.
pub fn remove_job(ctx: &JobContext<'_>, fleet: &str, crew: &str) -> Result<Job, DesiredError> {
    let mut args = strings(&["crew-remove", "--crew"]);
    args.push(format!("{fleet}/{crew}"));
    job(
        ctx,
        Parts {
            name: names::remove_job(fleet, crew),
            owned: false,
            component: "remove",
            extra_labels: vec![
                ("balerix.ai/fleet", fleet.to_string()),
                ("balerix.ai/crew", crew.to_string()),
            ],
            args,
            slices: vec![
                slice(names::vol_crew_repo(fleet, crew), "repo", true),
                slice(names::vol_crew_pool(fleet, crew), "crew", true),
            ],
            volumes: vec![],
            mounts: vec![],
            also_hashed: Value::Null,
            claim_dir: None,
        },
    )
}

/// `pools/daemon`: the system table this image embeds (§20.3).
pub fn daemon_pool_job(ctx: &JobContext<'_>) -> Result<Job, DesiredError> {
    job(
        ctx,
        Parts {
            name: names::daemon_pool_job(ctx.daemon),
            owned: true,
            component: "pool",
            extra_labels: vec![],
            args: strings(&["pool-sync", "--level", "daemon"]),
            slices: vec![slice(names::vol_daemon_pool(), "daemon", true)],
            volumes: vec![],
            mounts: vec![],
            also_hashed: Value::Null,
            claim_dir: None,
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
            owned: true,
            component: "pool",
            extra_labels: vec![("balerix.ai/fleet", fleet.to_string())],
            args,
            slices: vec![
                slice(names::vol_fleet_pool(fleet), "fleet", true),
                slice(names::vol_daemon_pool(), "daemon", false),
            ],
            volumes: vec![],
            mounts: vec![],
            also_hashed: Value::Null,
            claim_dir: None,
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
            owned: true,
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
            also_hashed: Value::Null,
            claim_dir: None,
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
    harvest(ctx, agent_name, agent, None)
}

/// A branch change (§27): the clone on the claim was made for `from`,
/// and the Agent's branch is now `to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranchMove<'a> {
    pub from: &'a str,
    pub to: &'a str,
    /// The Agent's `balerix.ai/purge`: the clone is removed unread.
    pub purge: bool,
}

/// The harvest Job of a branch change (§27.2), by the harvest Job's name
/// and labels: `harvest --branch <to>` settles the clone as the pod's
/// `ensure_clone` would, with the cache writable, so the claim is
/// mounted read-write: the old branch harvested and the clone removed,
/// or the clone kept where a host keeps it. Its hash also holds `from`,
/// so it is never a removal's harvest, nor the change before it.
pub fn branch_change_job(
    ctx: &JobContext<'_>,
    agent_name: &str,
    agent: &AgentSpec,
    change: &BranchMove<'_>,
) -> Result<Job, DesiredError> {
    harvest(ctx, agent_name, agent, Some(change))
}

fn harvest(
    ctx: &JobContext<'_>,
    agent_name: &str,
    agent: &AgentSpec,
    change: Option<&BranchMove<'_>>,
) -> Result<Job, DesiredError> {
    let mut args = strings(&["harvest", "--agent"]);
    args.push(format!("{}/{}/{}", agent.fleet, agent.crew, agent.agent));
    let mut also_hashed = Value::Null;
    if let Some(change) = change {
        args.extend(["--branch".to_string(), change.to.to_string()]);
        if change.purge {
            args.push("--purge".to_string());
        }
        also_hashed = json!({ "from": change.from });
    }
    // the claim read-only; a branch change's clone alone read-write,
    // where the Job empties it (§27.2)
    let mut mounts = vec![
        json!({ "name": "agent", "mountPath": "/balerix/agent", "subPath": AGENT_DIR, "readOnly": true }),
    ];
    let claim_dir = change.map(|_| "workspace".to_string());
    if change.is_some() {
        mounts.push(json!({ "name": "agent", "mountPath": "/balerix/agent/workspace", "subPath": format!("{AGENT_DIR}/workspace"), "readOnly": false }));
    }
    job(
        ctx,
        Parts {
            name: names::harvest_job(agent_name),
            owned: false,
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
                "persistentVolumeClaim": { "claimName": agent_name, "readOnly": change.is_none() },
            })],
            mounts,
            also_hashed,
            claim_dir,
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

/// A failed Job with no pod message: its `Failed` condition's reason and
/// message (`DeadlineExceeded: Job was active longer than specified
/// deadline`) when the Job controller wrote them.
fn failed_condition(job: &Job) -> String {
    let condition = job
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .and_then(|c| c.iter().find(|c| c.type_ == "Failed" && c.status == "True"))
        .filter(|c| c.reason.as_deref().is_some_and(|r| !r.is_empty()));
    match condition {
        Some(c) => format!(
            "the job failed: {}: {}",
            c.reason.as_deref().unwrap_or_default(),
            c.message.as_deref().unwrap_or_default()
        ),
        None => "the job failed and left no message".to_string(),
    }
}

/// Whether a Job failed: a pod counted as failed, or the Job controller's
/// `Failed` condition. A Job whose pod was never created (a ResourceQuota,
/// an admission webhook) reaches `Failed=True/DeadlineExceeded` under
/// `activeDeadlineSeconds` with `failed: 0` (§22.3).
pub fn job_failed(job: &Job) -> bool {
    let status = job.status.as_ref();
    status.and_then(|s| s.failed).unwrap_or(0) >= 1
        || status
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|c| c.iter().any(|c| c.type_ == "Failed" && c.status == "True"))
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
    if job_failed(job) {
        return JobOutcome::Failed(message(pods).unwrap_or_else(|| failed_condition(job)));
    }
    JobOutcome::Running
}

/// `CacheReady` and `ToolsReady` (§4.3). `CacheReady` follows the crew's
/// sync Job alone; `ToolsReady` follows the fleet's pool Job, then the
/// sync Job's tools half. A sync that failed says which half in its
/// message's prefix (`cache:` or `tools:`); the tools half runs after the
/// cache, so a `tools:` failure means the cache is synced. `cacheRef` is
/// the commit the last successful sync reported, and stays until the next
/// one.
pub fn crew_status(
    crew: &Crew,
    fleet_pool: &JobOutcome,
    sync: &JobOutcome,
    now: &Time,
) -> CrewStatus {
    // the cache follows the sync Job alone; the tools follow the fleet's
    // pool Job, then the sync Job's tools half
    let (cache, commit) = match sync {
        JobOutcome::Succeeded(commit) => (Cond::yes("CacheReady", "Synced", ""), Some(commit)),
        JobOutcome::Failed(message) if message.starts_with("tools:") => {
            (Cond::yes("CacheReady", "Synced", ""), None)
        }
        JobOutcome::Failed(message) => (Cond::no("CacheReady", "SyncFailed", message), None),
        JobOutcome::Running | JobOutcome::Stale | JobOutcome::Absent => {
            (Cond::no("CacheReady", "Syncing", ""), None)
        }
    };
    let tools = match (fleet_pool, sync) {
        (JobOutcome::Failed(message), _) => Cond::no("ToolsReady", "FleetPoolFailed", message),
        (JobOutcome::Running | JobOutcome::Stale | JobOutcome::Absent, _) => {
            Cond::no("ToolsReady", "PoolSyncRunning", "")
        }
        (JobOutcome::Succeeded(_), JobOutcome::Succeeded(_)) => {
            Cond::yes("ToolsReady", "Synced", "")
        }
        (JobOutcome::Succeeded(_), JobOutcome::Failed(message))
            if message.starts_with("tools:") =>
        {
            Cond::no("ToolsReady", "SyncFailed", message)
        }
        // the cache half failed, so the tools half never ran
        (JobOutcome::Succeeded(_), JobOutcome::Failed(_)) => {
            Cond::unknown("ToolsReady", "SyncFailed", "")
        }
        (
            JobOutcome::Succeeded(_),
            JobOutcome::Running | JobOutcome::Stale | JobOutcome::Absent,
        ) => Cond::no("ToolsReady", "Syncing", ""),
    };
    let old = crew.status.as_ref();
    CrewStatus {
        observed_generation: crew.metadata.generation,
        conditions: conditions(
            // a Crew with no status yet has no conditions
            old.map(|s| s.conditions.as_slice()).unwrap_or_default(),
            &[cache, tools],
            crew.metadata.generation,
            now,
        ),
        // a sync that reported no commit (its pod gone) keeps the old one
        cache_ref: commit
            .filter(|c| !c.is_empty())
            .cloned()
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
        // an unowned Job's is also its owner's uid: one made for an
        // earlier owner of the same name is stale (§26.3)
        let mut later = ctx(&images);
        later.owner["uid"] = json!("another-uid");
        assert_ne!(
            hash_of(&harvest_job(&c, "payments-backend-alice", &agent()).unwrap()),
            hash_of(&harvest_job(&later, "payments-backend-alice", &agent()).unwrap())
        );
        assert_ne!(
            hash_of(&remove_job(&c, "payments", "backend").unwrap()),
            hash_of(&remove_job(&later, "payments", "backend").unwrap())
        );
        // an owned Job's is not: its owner reference already says whose
        assert_eq!(
            hash_of(&crew_sync_job(&c, &crew("gh"), Some("gh-token")).unwrap()),
            hash_of(&crew_sync_job(&later, &crew("gh"), Some("gh-token")).unwrap())
        );
    }

    /// §27.2: a branch change's harvest is the harvest Job by name and
    /// labels, with the new branch in its args and the claim writable;
    /// its hash is never a removal's, the change before it, or its
    /// purged form's.
    #[test]
    fn a_branch_changes_harvest_names_its_branch_and_hashes_the_old_one() {
        let images = images();
        let c = ctx(&images);
        let hash_of =
            |job: &Job| job.metadata.annotations.as_ref().unwrap()[HASH_ANNOTATION].clone();
        let change = |from, to, purge| {
            branch_change_job(
                &c,
                "payments-backend-alice",
                &agent(),
                &BranchMove { from, to, purge },
            )
            .unwrap()
        };
        let removal = harvest_job(&c, "payments-backend-alice", &agent()).unwrap();
        let a_to_b = change("a", "b", false);
        assert_eq!(a_to_b.metadata.name, removal.metadata.name);
        assert_eq!(a_to_b.metadata.labels, removal.metadata.labels);
        let pod = serde_json::to_value(&a_to_b.spec.as_ref().unwrap().template.spec).unwrap();
        assert_eq!(
            pod["containers"][0]["args"],
            json!([
                "harvest",
                "--agent",
                "payments/backend/alice",
                "--branch",
                "b"
            ])
        );
        // the claim read-only, but for its clone (§27.2)
        let agent_mounts: Vec<_> = pod["containers"][0]["volumeMounts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["name"] == "agent")
            .map(|m| {
                (
                    m["mountPath"].clone(),
                    m["subPath"].clone(),
                    m["readOnly"].clone(),
                )
            })
            .collect();
        assert_eq!(
            agent_mounts,
            [
                (json!("/balerix/agent"), json!("agent"), json!(true)),
                (
                    json!("/balerix/agent/workspace"),
                    json!("agent/workspace"),
                    json!(false)
                ),
            ]
        );
        let init = &pod["initContainers"][0];
        assert_eq!(
            init["command"].as_array().unwrap().last().unwrap(),
            "/balerix/claim/workspace"
        );
        // the claim's `agent/` alone, never the whole claim
        assert!(init["volumeMounts"].as_array().unwrap().contains(
            &json!({ "name": "agent", "mountPath": "/balerix/claim", "subPath": "agent" })
        ));
        insta::assert_yaml_snapshot!("job_branch_change", a_to_b);
        assert_eq!(
            a_to_b
                .spec
                .as_ref()
                .unwrap()
                .ttl_seconds_after_finished
                .map(i64::from),
            Some(FINISHED_TTL_SECONDS)
        );
        assert_eq!(a_to_b.metadata.owner_references, None);

        assert_eq!(hash_of(&a_to_b), hash_of(&change("a", "b", false)));
        assert_ne!(hash_of(&a_to_b), hash_of(&removal));
        // a -> b, b -> a, a -> b: each the one before's successor is
        // stale, never read as its own success
        assert_ne!(hash_of(&a_to_b), hash_of(&change("b", "a", false)));
        // the same new branch from another old one
        assert_ne!(hash_of(&a_to_b), hash_of(&change("c", "b", false)));
        // the purge replaces a failed harvest
        let purged = change("a", "b", true);
        assert_ne!(hash_of(&a_to_b), hash_of(&purged));
        let args = serde_json::to_value(&purged.spec.as_ref().unwrap().template.spec).unwrap()
            ["containers"][0]["args"]
            .clone();
        assert_eq!(args.as_array().unwrap().last().unwrap(), "--purge");
        // and its owner's uid, as any unowned Job's (§26.3)
        let mut later = ctx(&images);
        later.owner["uid"] = json!("another-uid");
        assert_ne!(
            hash_of(&a_to_b),
            hash_of(
                &branch_change_job(
                    &later,
                    "payments-backend-alice",
                    &agent(),
                    &BranchMove {
                        from: "a",
                        to: "b",
                        purge: false
                    }
                )
                .unwrap()
            )
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
    fn a_job_whose_pod_was_never_created_fails_by_its_condition() {
        let images = images();
        let wanted = daemon_pool_job(&ctx(&images)).unwrap();
        // a quota refused the pod; the deadline failed the Job: `failed` is 0
        let refused = observed(
            &wanted,
            json!({ "conditions": [{ "type": "Failed", "status": "True",
                "reason": "DeadlineExceeded", "message": "Job was active longer than specified deadline" }] }),
        );
        assert!(job_failed(&refused));
        assert_eq!(
            job_outcome(Some(&refused), &[], &wanted),
            JobOutcome::Failed(
                "the job failed: DeadlineExceeded: Job was active longer than specified deadline"
                    .into()
            )
        );
        // a condition that is not True decides nothing
        let not_yet = observed(
            &wanted,
            json!({ "conditions": [{ "type": "Failed", "status": "False" }] }),
        );
        assert!(!job_failed(&not_yet));
        assert_eq!(
            job_outcome(Some(&not_yet), &[], &wanted),
            JobOutcome::Running
        );
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

        // past its deadline with no pod to speak: the Job's own condition
        let expired = observed(
            &wanted,
            json!({ "failed": 1, "conditions": [{ "type": "Failed", "status": "True",
                "reason": "DeadlineExceeded", "message": "Job was active longer than specified deadline" }] }),
        );
        assert_eq!(
            job_outcome(Some(&expired), &[], &wanted),
            JobOutcome::Failed(
                "the job failed: DeadlineExceeded: Job was active longer than specified deadline"
                    .into()
            )
        );
        assert_eq!(
            wanted.spec.as_ref().unwrap().active_deadline_seconds,
            Some(JOB_DEADLINE_SECONDS)
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

        // a success whose pod is gone reports no commit: the old one stays
        let gone = JobOutcome::Succeeded(String::new());
        let s = crew_status(&crew_object(Some("old")), &ok, &gone, &now);
        assert_eq!(s.cache_ref.as_deref(), Some("old"));

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

        // the cache follows the sync Job alone: a fleet pool still running,
        // or stale, beside a sync that succeeded leaves the cache ready
        for pool in [JobOutcome::Running, JobOutcome::Stale, JobOutcome::Absent] {
            let s = crew_status(
                &crew_object(None),
                &pool,
                &JobOutcome::Succeeded("abc123".into()),
                &now,
            );
            assert_eq!(
                crew_cond(&s, "CacheReady"),
                ("True", "Synced", ""),
                "{pool:?}"
            );
            assert_eq!(s.cache_ref.as_deref(), Some("abc123"), "{pool:?}");
            assert_eq!(
                crew_cond(&s, "ToolsReady"),
                ("False", "PoolSyncRunning", ""),
                "{pool:?}"
            );
        }
        // and a cache failure is reported beside a fleet pool that failed
        let s = crew_status(
            &crew_object(None),
            &JobOutcome::Failed("tools: payments: fleet payments: boom".into()),
            &cache,
            &now,
        );
        assert_eq!(
            crew_cond(&s, "CacheReady"),
            (
                "False",
                "SyncFailed",
                "cache: payments/backend: the remote has no branch nope"
            )
        );
        assert_eq!(crew_cond(&s, "ToolsReady").1, "FleetPoolFailed");
        // a `tools:` failure of the sync means the cache is synced, whatever the pool
        let s = crew_status(&crew_object(None), &JobOutcome::Running, &tools, &now);
        assert_eq!(crew_cond(&s, "CacheReady"), ("True", "Synced", ""));
        assert_eq!(
            crew_cond(&s, "ToolsReady"),
            ("False", "PoolSyncRunning", "")
        );
    }

    #[test]
    fn the_remove_job_runs_crew_remove_over_the_crews_writable_slice() {
        let images = images();
        let job = remove_job(&ctx(&images), "f", "c").unwrap();
        insta::assert_yaml_snapshot!(job);
    }

    #[test]
    fn every_job_mounts_an_empty_dir_at_tmp() {
        let images = images();
        let job = daemon_pool_job(&ctx(&images)).unwrap();
        let spec = job.spec.unwrap().template.spec.unwrap();
        let mounts = spec.containers[0].volume_mounts.clone().unwrap();
        assert!(
            mounts
                .iter()
                .any(|m| m.mount_path == "/tmp" && m.name == "tmp")
        );
        assert!(
            spec.volumes
                .unwrap()
                .iter()
                .any(|v| v.name == "tmp" && v.empty_dir.is_some())
        );
    }
}
