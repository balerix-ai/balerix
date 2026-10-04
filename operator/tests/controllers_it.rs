#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The controllers against a real API server (Spec O §21.3). The test is
//! the kubelet: it patches Job and Pod status and force-deletes pods.
mod support;

use std::time::Duration;

use balerix_operator::api::{Daemon, DaemonSpec};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::Api;
use kube::api::{Patch, PatchParams, PostParams};
use support::envtest::envtest;
use support::{TestClock, finish_job, hold_for, namespace, spawn_operator, wait_for};

fn daemon_spec() -> DaemonSpec {
    serde_json::from_value(serde_json::json!({
        "storage": {
            "state": { "size": "1Gi" },
            "shared": { "size": "10Gi" },
            "agent": { "size": "2Gi" }
        }
    }))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_harness_serves_the_kinds_and_the_operator_starts_and_stops() {
    let Some(env) = envtest().await else { return };
    let ns = namespace(&env.client, "harness").await;
    let daemons: Api<Daemon> = Api::namespaced(env.client.clone(), &ns);
    daemons
        .create(
            &PostParams::default(),
            &Daemon::new("default", daemon_spec()),
        )
        .await
        .unwrap();
    let clock = TestClock::default();
    let operator = spawn_operator(env, &ns, None, &clock);
    let got = wait_for(
        "the Daemon to be listed",
        Duration::from_secs(10),
        || async { daemons.get_opt("default").await.unwrap() },
    )
    .await;
    assert_eq!(got.spec, daemon_spec());
    tokio::time::sleep(Duration::from_millis(500)).await;
    operator.abort();
}

fn condition<'a>(
    conditions: &'a [k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
    type_: &str,
) -> &'a k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition {
    conditions
        .iter()
        .find(|c| c.type_ == type_)
        .unwrap_or_else(|| panic!("no condition {type_}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_gets_its_objects_and_a_renewal_rolls_the_pod() {
    let Some(env) = envtest().await else { return };
    let ns = namespace(&env.client, "daemon").await;
    let client = env.client.clone();
    let daemons: Api<Daemon> = Api::namespaced(client.clone(), &ns);
    daemons
        .create(
            &PostParams::default(),
            &Daemon::new("default", daemon_spec()),
        )
        .await
        .unwrap();
    let clock = TestClock::default();
    let operator = spawn_operator(env, &ns, None, &clock);

    let sts = wait_for("the StatefulSet", Duration::from_secs(30), || async {
        Api::<StatefulSet>::namespaced(client.clone(), &ns)
            .get_opt("balerix-default")
            .await
            .unwrap()
    })
    .await;
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &ns);
    let ca = secrets.get("balerix-default-ca").await.unwrap();
    assert!(
        ca.data.as_ref().unwrap().contains_key("ca.crt")
            && ca.data.as_ref().unwrap().contains_key("ca.key")
    );
    let tls = secrets.get("balerix-default-tls").await.unwrap();
    assert_eq!(tls.type_.as_deref(), Some("kubernetes.io/tls"));
    let not_after: i64 = tls.metadata.annotations.as_ref().unwrap()["balerix.ai/not-after"]
        .parse()
        .unwrap();
    let admin = secrets.get("balerix-default-admin").await.unwrap();
    assert_eq!(admin.data.as_ref().unwrap()["token"].0.len(), 64);
    assert!(
        Api::<ConfigMap>::namespaced(client.clone(), &ns)
            .get("balerix-default-ca")
            .await
            .unwrap()
            .data
            .unwrap()
            .contains_key("ca.crt")
    );
    Api::<Service>::namespaced(client.clone(), &ns)
        .get("balerix-default")
        .await
        .unwrap();
    Api::<NetworkPolicy>::namespaced(client.clone(), &ns)
        .get("balerix-default")
        .await
        .unwrap();
    Api::<Job>::namespaced(client.clone(), &ns)
        .get("balerix-default-pool")
        .await
        .unwrap();
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
    claims.get("balerix-default-state").await.unwrap();
    claims.get("balerix-default-shared").await.unwrap();
    assert_eq!(
        sts.spec
            .as_ref()
            .unwrap()
            .template
            .metadata
            .as_ref()
            .unwrap()
            .annotations
            .as_ref()
            .unwrap()["balerix.ai/not-after"],
        not_after.to_string()
    );

    let status = wait_for("status", Duration::from_secs(10), || async {
        daemons
            .get("default")
            .await
            .unwrap()
            .status
            .filter(|s| !s.conditions.is_empty())
    })
    .await;
    assert_eq!(
        status.endpoint.as_deref(),
        Some(format!("https://balerix-default.{ns}.svc:7643").as_str())
    );
    let storage = condition(&status.conditions, "StorageReady");
    assert_eq!(
        (storage.status.as_str(), storage.reason.as_str()),
        ("False", "ClaimPending")
    );
    assert_eq!(condition(&status.conditions, "Ready").status, "False");
    assert_eq!(
        condition(&status.conditions, "SystemToolsReady").reason,
        "PoolSyncRunning"
    );

    // the serving certificate expires in ten days: inside the renewal window
    let soon = clock.clock()() + 10 * 86_400;
    secrets
        .patch(
            "balerix-default-tls",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "metadata": { "annotations": { "balerix.ai/not-after": soon.to_string() } }
            })),
        )
        .await
        .unwrap();
    let renewed = wait_for("a renewed certificate", Duration::from_secs(30), || async {
        let s = secrets.get("balerix-default-tls").await.unwrap();
        let t: i64 = s.metadata.annotations.as_ref().unwrap()["balerix.ai/not-after"]
            .parse()
            .unwrap();
        (t > soon + 60 * 86_400).then_some((t, s))
    })
    .await;
    assert_ne!(
        renewed.1.data.as_ref().unwrap()["tls.crt"],
        tls.data.as_ref().unwrap()["tls.crt"]
    );
    wait_for(
        "the pod template to roll",
        Duration::from_secs(30),
        || async {
            let s = Api::<StatefulSet>::namespaced(client.clone(), &ns)
                .get("balerix-default")
                .await
                .unwrap();
            (s.spec
                .unwrap()
                .template
                .metadata
                .unwrap()
                .annotations
                .unwrap()["balerix.ai/not-after"]
                == renewed.0.to_string())
            .then_some(())
        },
    )
    .await;
    // the authority and the admin token are kept
    assert_eq!(
        secrets.get("balerix-default-ca").await.unwrap().data,
        ca.data
    );
    assert_eq!(
        secrets.get("balerix-default-admin").await.unwrap().data,
        admin.data
    );
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_pool_job_is_reported_and_retried_after_the_delay() {
    let Some(env) = envtest().await else { return };
    let ns = namespace(&env.client, "pooljob").await;
    let client = env.client.clone();
    let daemons: Api<Daemon> = Api::namespaced(client.clone(), &ns);
    daemons
        .create(
            &PostParams::default(),
            &Daemon::new("default", daemon_spec()),
        )
        .await
        .unwrap();
    let clock = TestClock::default();
    let operator = spawn_operator(env, &ns, None, &clock);
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let first = wait_for("the pool Job", Duration::from_secs(30), || async {
        jobs.get_opt("balerix-default-pool").await.unwrap()
    })
    .await;

    // the kubelet ran it, it failed, and its pod is already gone
    finish_job(&client, &ns, "balerix-default-pool", false, None).await;
    let status = wait_for(
        "SystemToolsReady=False for the failure",
        Duration::from_secs(10),
        || async {
            // `PoolSyncRunning` is False too: wait for the failure's reason
            let s = daemons.get("default").await.unwrap().status?;
            let tools = condition(&s.conditions, "SystemToolsReady");
            (tools.status == "False" && tools.reason == "PoolSyncFailed").then_some(s)
        },
    )
    .await;
    let tools = condition(&status.conditions, "SystemToolsReady");
    assert_eq!(tools.reason, "PoolSyncFailed");
    assert_eq!(tools.message, "the job failed and left no message");
    // not retried before the delay
    hold_for("a retry before 30 s", Duration::from_secs(3), || async {
        (jobs.get("balerix-default-pool").await.unwrap().metadata.uid != first.metadata.uid)
            .then_some(())
    })
    .await;

    clock.advance(31);
    let second = wait_for("the retry", Duration::from_secs(30), || async {
        let j = jobs.get_opt("balerix-default-pool").await.unwrap()?;
        (j.metadata.uid != first.metadata.uid).then_some(j)
    })
    .await;
    let attempts = daemons
        .get("default")
        .await
        .unwrap()
        .metadata
        .annotations
        .unwrap()["balerix.ai/attempts"]
        .clone();
    assert_eq!(attempts, "{\"balerix-default-pool\":2}");

    finish_job(&client, &ns, "balerix-default-pool", true, Some("synced")).await;
    wait_for("SystemToolsReady=True", Duration::from_secs(10), || async {
        let s = daemons.get("default").await.unwrap().status?;
        (condition(&s.conditions, "SystemToolsReady").status == "True").then_some(())
    })
    .await;
    assert_eq!(
        condition(
            &daemons
                .get("default")
                .await
                .unwrap()
                .status
                .unwrap()
                .conditions,
            "SystemToolsReady"
        )
        .reason,
        "PoolSynced"
    );
    let attempts = daemons
        .get("default")
        .await
        .unwrap()
        .metadata
        .annotations
        .unwrap_or_default()
        .get("balerix.ai/attempts")
        .cloned();
    assert!(
        attempts.is_none() || attempts.as_deref() == Some("{}"),
        "{attempts:?}"
    );
    let _ = second;
    operator.abort();
}
