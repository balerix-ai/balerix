//! The `balerix` binary `scripts/operator.sh` built, a temp root under
//! `target/tmp`, and the controllers' test support: a namespace per test,
//! polling, a clock the test moves, the operator as a task.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

pub mod envtest;
pub mod stub_daemon;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use balerix_operator::controllers::{Clock, RunConfig};
use balerix_operator::desired::common::Images;
use k8s_openapi::api::core::v1::Namespace;
use kube::api::PostParams;
use kube::{Api, Client};

/// From `BALERIX_BIN`; `None` after printing a skip (a failure under
/// `BALERIX_REQUIRE_TOOLS=1`).
pub fn balerix() -> Option<PathBuf> {
    let found = std::env::var_os("BALERIX_BIN")
        .map(PathBuf::from)
        .filter(|p| p.is_file());
    if found.is_none() {
        if std::env::var("BALERIX_REQUIRE_TOOLS").as_deref() == Ok("1") {
            panic!(
                "BALERIX_BIN missing and BALERIX_REQUIRE_TOOLS=1 (run through scripts/operator.sh)"
            );
        }
        eprintln!("skip: BALERIX_BIN missing");
    }
    found
}

pub fn temp_root(label: &str) -> PathBuf {
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// A namespace of this test's own: `t-<label>-<pid>`.
pub async fn namespace(client: &Client, label: &str) -> String {
    let name = format!("t-{label}-{}", std::process::id());
    let ns: Namespace = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": name }
    }))
    .unwrap();
    Api::<Namespace>::all(client.clone())
        .create(&PostParams::default(), &ns)
        .await
        .unwrap();
    name
}

/// How long one probe of `wait_for` or `hold_for` may take. A kube request
/// is now and then lost on a pooled connection and never answered (the
/// client has no read timeout); the probe is dropped and polled again.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// One probe: `Some(answer)`, or `None` when it was lost.
async fn probe_once<T, Fut>(probe: Fut) -> Option<Option<T>>
where
    Fut: std::future::Future<Output = Option<T>>,
{
    let answer = tokio::time::timeout(PROBE_TIMEOUT, probe).await.ok();
    if answer.is_none() {
        eprintln!("a probe took over {PROBE_TIMEOUT:?}: dropped");
    }
    answer
}

/// Polls `probe` every 200 ms until it answers `Some`, or panics with
/// `what` after `timeout`. A lost probe is "not yet".
pub async fn wait_for<T, F, Fut>(what: &str, timeout: Duration, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(Some(t)) = probe_once(probe()).await {
            return t;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Holds `what` true for `hold`: panics the first time `probe` answers `Some`.
/// A lost probe saw nothing, so it proves nothing: the hold ends only on a
/// probe that answered after `hold`, and panics if none has answered
/// `PROBE_TIMEOUT` × 6 past it.
pub async fn hold_for<T, F, Fut>(what: &str, hold: Duration, mut probe: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + hold;
    let give_up = deadline + PROBE_TIMEOUT * 6;
    loop {
        match probe_once(probe()).await {
            Some(answer) => {
                assert!(answer.is_none(), "{what} happened; it must not");
                if Instant::now() >= deadline {
                    return;
                }
            }
            None => assert!(
                Instant::now() < give_up,
                "no probe of {what} answered after the hold"
            ),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The system clock plus an offset the test moves.
#[derive(Clone, Default)]
pub struct TestClock {
    pub offset: Arc<AtomicI64>,
}

impl TestClock {
    pub fn clock(&self) -> Clock {
        let offset = self.offset.clone();
        Arc::new(move || {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            now + offset.load(Ordering::SeqCst)
        })
    }
    pub fn advance(&self, secs: i64) {
        self.offset.fetch_add(secs, Ordering::SeqCst);
    }
}

/// The controllers over one namespace, as a task the test aborts at its end.
pub fn spawn_operator(
    env: &envtest::EnvTest,
    namespace: &str,
    daemon_url: Option<String>,
    clock: &TestClock,
) -> tokio::task::AbortHandle {
    let mut cfg = RunConfig::new(
        "0.2.0",
        Images {
            daemon: "balerix:test".into(),
            agent: "balerix-agent:test".into(),
        },
        namespace,
    );
    cfg.watch_namespaces = Some(vec![namespace.to_string()]);
    cfg.fleet_period = Duration::from_secs(1);
    cfg.period = Duration::from_secs(1);
    cfg.clock = clock.clock();
    cfg.insecure_daemon_url = daemon_url;
    tokio::spawn(balerix_operator::controllers::run(
        env.client.clone(),
        env.watches.clone(),
        cfg,
    ))
    .abort_handle()
}

/// The kubelet's part for a Job in envtest: marks it succeeded or failed
/// (with a `Failed` condition stamped now), and when `message` is given,
/// leaves a pod labelled `job-name` whose container terminated with it.
/// The pod comes first and the Job's status last: the Job's patch wakes
/// the operator at once, pods are not watched, and a reconcile that ran
/// before the message existed would report the wrong one until the next
/// requeue.
pub async fn finish_job(
    client: &Client,
    namespace: &str,
    name: &str,
    succeeded: bool,
    message: Option<&str>,
) {
    use k8s_openapi::api::batch::v1::Job;
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{Patch, PatchParams};
    let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);
    let now = k8s_openapi::jiff::Timestamp::now().to_string();
    // Kubernetes 1.34 refuses a finished Job without `startTime`, a
    // `Complete=True` without `completionTime` and `SuccessCriteriaMet=True`,
    // and a `Failed=True` without `FailureTarget=True`
    let status = if succeeded {
        serde_json::json!({ "status": { "startTime": now, "completionTime": now, "succeeded": 1, "conditions": [
            { "type": "SuccessCriteriaMet", "status": "True", "lastTransitionTime": now },
            { "type": "Complete", "status": "True", "lastTransitionTime": now }
        ] } })
    } else {
        serde_json::json!({ "status": { "startTime": now, "failed": 1, "conditions": [
            { "type": "FailureTarget", "status": "True", "lastTransitionTime": now },
            { "type": "Failed", "status": "True", "lastTransitionTime": now }
        ] } })
    };
    if let Some(message) = message {
        let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
        let pod_name = format!("{name}-pod");
        // the operator reads a Job's pods by its uid; a pod reused across a
        // retry follows the new Job's
        let uid = jobs.get(name).await.unwrap().metadata.uid.unwrap();
        let labels =
            serde_json::json!({ "job-name": name, "batch.kubernetes.io/controller-uid": uid });
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": { "name": pod_name, "namespace": namespace, "labels": labels },
            "spec": { "containers": [{ "name": "job", "image": "x" }], "restartPolicy": "Never" }
        }))
        .unwrap();
        if pods.get_opt(&pod_name).await.unwrap().is_none() {
            pods.create(&PostParams::default(), &pod).await.unwrap();
        } else {
            pods.patch(
                &pod_name,
                &PatchParams::default(),
                &Patch::Merge(&serde_json::json!({
                    "metadata": { "labels": labels }
                })),
            )
            .await
            .unwrap();
        }
        let phase = if succeeded { "Succeeded" } else { "Failed" };
        pods.patch_status(&pod_name, &PatchParams::default(), &Patch::Merge(&serde_json::json!({
            "status": { "phase": phase, "containerStatuses": [{
                "name": "job", "image": "x", "imageID": "x", "ready": false, "restartCount": 0,
                "state": { "terminated": { "exitCode": if succeeded { 0 } else { 1 }, "message": message } }
            }] }
        }))).await.unwrap();
    }
    jobs.patch_status(name, &PatchParams::default(), &Patch::Merge(&status))
        .await
        .unwrap();
}

/// The kubelet's part for a deleted Pod in envtest: once the operator has
/// set its deletion timestamp, remove it with grace period 0. Returns
/// when the Pod that exists now is gone (a new one by the same name may
/// already stand in its place: a Pod with no node is deleted at once,
/// and the operator makes the next at once). Panics after `timeout` if
/// it was never deleted.
pub async fn reap_pod(client: &Client, namespace: &str, name: &str, timeout: Duration) {
    use k8s_openapi::api::core::v1::Pod;
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let Some(uid) = pods
        .get_opt(name)
        .await
        .unwrap()
        .and_then(|p| p.metadata.uid)
    else {
        return;
    };
    reap_pod_uid(client, namespace, name, &uid, timeout).await;
}

/// `reap_pod` for the Pod with this uid, which the caller saw earlier.
pub async fn reap_pod_uid(
    client: &Client,
    namespace: &str,
    name: &str,
    uid: &str,
    timeout: Duration,
) {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{DeleteParams, Preconditions};
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    wait_for(&format!("pod {name} to be deleted"), timeout, || async {
        match pods.get_opt(name).await.unwrap() {
            Some(p) if p.metadata.uid.as_deref() == Some(uid) => {
                if p.metadata.deletion_timestamp.is_some() {
                    let params = DeleteParams {
                        grace_period_seconds: Some(0),
                        preconditions: Some(Preconditions {
                            uid: Some(uid.to_string()),
                            resource_version: None,
                        }),
                        ..DeleteParams::default()
                    };
                    let _ = pods.delete(name, &params).await;
                }
                None
            }
            // gone, or replaced
            _ => Some(()),
        }
    })
    .await;
}

/// The controller-manager's part for a deleted claim in envtest: the
/// `kubernetes.io/pvc-protection` finalizer comes off once the claim has
/// a deletion timestamp. Returns when the claim is gone; panics after
/// `timeout` if it was never deleted.
pub async fn reap_claim(client: &Client, namespace: &str, name: &str, timeout: Duration) {
    use k8s_openapi::api::core::v1::PersistentVolumeClaim;
    use kube::api::{Patch, PatchParams};
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), namespace);
    wait_for(&format!("claim {name} gone"), timeout, || async {
        match claims.get_opt(name).await.unwrap() {
            None => Some(()),
            Some(c) if c.metadata.deletion_timestamp.is_some() => {
                let _ = claims
                    .patch(
                        name,
                        &PatchParams::default(),
                        &Patch::Merge(serde_json::json!({ "metadata": { "finalizers": null } })),
                    )
                    .await;
                None
            }
            Some(_) => None,
        }
    })
    .await;
}

/// The garbage collector's part for a Job deleted in the foreground in
/// envtest: once the Job with this uid has a deletion timestamp, its
/// `foregroundDeletion` finalizer comes off (its pods, if any, are the
/// test's to have deleted). Returns when that Job is gone or replaced;
/// panics after `timeout` if it was never deleted.
pub async fn reap_job(client: &Client, namespace: &str, name: &str, uid: &str, timeout: Duration) {
    use k8s_openapi::api::batch::v1::Job;
    use kube::api::{Patch, PatchParams};
    let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);
    wait_for(&format!("Job {name} deleted"), timeout, || async {
        match jobs.get_opt(name).await.unwrap() {
            Some(j) if j.metadata.uid.as_deref() == Some(uid) => {
                if j.metadata.deletion_timestamp.is_some() {
                    let _ = jobs
                        .patch(
                            name,
                            &PatchParams::default(),
                            &Patch::Merge(
                                serde_json::json!({ "metadata": { "finalizers": null } }),
                            ),
                        )
                        .await;
                }
                None
            }
            _ => Some(()),
        }
    })
    .await;
}
