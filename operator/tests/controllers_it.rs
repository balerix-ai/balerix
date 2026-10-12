#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The controllers against a real API server (Spec O §21.3). The test is
//! the kubelet: it patches Job and Pod status and force-deletes pods.
mod support;

use std::time::Duration;

use balerix_api::{AgentPhase, AgentStatus as DaemonAgentStatus};
use balerix_operator::api::{Agent, Crew, Daemon, DaemonSpec, Fleet, FleetSpec};
use balerix_operator::desired::daemon::{SERVING_CERT_ANNOTATION, serving_cert_hash};
use balerix_operator::pki;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Pod, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::Api;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use support::envtest::envtest;
use support::stub_daemon::StubDaemon;
use support::{
    TestClock, expire_job, finish_job, hold_for, make_daemon_ready, namespace, reap_claim,
    reap_job, reap_pod, reap_pod_uid, spawn_operator, wait_for,
};

/// What the Daemon's pod template rolls on: its serving certificate.
fn template_cert(s: StatefulSet) -> String {
    s.spec
        .unwrap()
        .template
        .metadata
        .unwrap()
        .annotations
        .unwrap()[SERVING_CERT_ANNOTATION]
        .clone()
}

/// `template_cert`'s value for the serving certificate in `tls`.
fn cert_of(tls: &Secret) -> String {
    serving_cert_hash(std::str::from_utf8(&tls.data.as_ref().unwrap()["tls.crt"].0).unwrap())
}

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

    let sts = wait_for("the StatefulSet", Duration::from_secs(60), || async {
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
    // the Secret keeps the expiry the renewal check reads
    let _: i64 = tls.metadata.annotations.as_ref().unwrap()["balerix.ai/not-after"]
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
    // the reconcile applies the NetworkPolicy after the StatefulSet (#215)
    wait_for("the NetworkPolicy", Duration::from_secs(60), || async {
        Api::<NetworkPolicy>::namespaced(client.clone(), &ns)
            .get_opt("balerix-default")
            .await
            .unwrap()
    })
    .await;
    // the reconcile applies the StatefulSet before it creates the pool Job
    let pool_jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    wait_for("the pool Job", Duration::from_secs(60), || async {
        pool_jobs.get_opt("balerix-default-pool").await.unwrap()
    })
    .await;
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
    claims.get("balerix-default-state").await.unwrap();
    claims.get("balerix-default-shared").await.unwrap();
    assert_eq!(template_cert(sts.clone()), cert_of(&tls));

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
    let expiring = Patch::Merge(serde_json::json!({
        "metadata": { "annotations": { "balerix.ai/not-after": soon.to_string() } }
    }));
    secrets
        .patch("balerix-default-tls", &PatchParams::default(), &expiring)
        .await
        .unwrap();
    // a reconcile that read the Secret before the patch applies the old
    // annotation back: wait for a new certificate, patching again until
    // the operator has seen the expiry
    let renewed = wait_for("a renewed certificate", Duration::from_secs(60), || async {
        let s = secrets.get("balerix-default-tls").await.unwrap();
        let t: i64 = s.metadata.annotations.as_ref().unwrap()["balerix.ai/not-after"]
            .parse()
            .unwrap();
        if s.data.as_ref().unwrap()["tls.crt"] == tls.data.as_ref().unwrap()["tls.crt"] {
            if t != soon {
                secrets
                    .patch("balerix-default-tls", &PatchParams::default(), &expiring)
                    .await
                    .unwrap();
            }
            return None;
        }
        (t > soon + 60 * 86_400).then_some((t, s))
    })
    .await;
    assert_ne!(
        renewed.1.data.as_ref().unwrap()["tls.crt"],
        tls.data.as_ref().unwrap()["tls.crt"]
    );
    wait_for(
        "the pod template to roll",
        Duration::from_secs(60),
        || async {
            let s = Api::<StatefulSet>::namespaced(client.clone(), &ns)
                .get("balerix-default")
                .await
                .unwrap();
            (template_cert(s) == cert_of(&renewed.1)).then_some(())
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

    // a lost authority is minted again, and the serving certificate with it
    secrets
        .delete("balerix-default-ca", &Default::default())
        .await
        .unwrap();
    // a reconcile that read the Secret before the delete applies the old
    // authority back: delete again while the old one is there
    wait_for("a new authority", Duration::from_secs(60), || async {
        let s = secrets.get_opt("balerix-default-ca").await.unwrap()?;
        if s.data.as_ref()?.get("ca.crt") == ca.data.as_ref().unwrap().get("ca.crt") {
            // gone already when the operator's own read raced this one
            let _ = secrets
                .delete("balerix-default-ca", &Default::default())
                .await;
            return None;
        }
        Some(())
    })
    .await;
    let reissued = wait_for(
        "a serving certificate from the new authority",
        Duration::from_secs(60),
        || async {
            let s = secrets.get("balerix-default-tls").await.unwrap();
            (s.data.as_ref().unwrap()["tls.crt"] != renewed.1.data.as_ref().unwrap()["tls.crt"])
                .then_some(s)
        },
    )
    .await;
    wait_for(
        "the pod template to follow",
        Duration::from_secs(60),
        || async {
            let s = Api::<StatefulSet>::namespaced(client.clone(), &ns)
                .get("balerix-default")
                .await
                .unwrap();
            (template_cert(s) == cert_of(&reissued)).then_some(())
        },
    )
    .await;
    assert_eq!(
        secrets.get("balerix-default-admin").await.unwrap().data,
        admin.data
    );
    operator.abort();
}

/// A reconcile that applied a new authority and was dropped before the
/// serving Secret leaves a certificate the authority cannot verify: the
/// next reconcile counts it as missing and reissues it (§21.2).
#[tokio::test(flavor = "multi_thread")]
async fn a_serving_certificate_the_authority_cannot_verify_is_reissued() {
    let Some(env) = envtest().await else { return };
    let ns = namespace(&env.client, "daemon").await;
    let client = env.client.clone();
    Api::<Daemon>::namespaced(client.clone(), &ns)
        .create(
            &PostParams::default(),
            &Daemon::new("default", daemon_spec()),
        )
        .await
        .unwrap();
    let clock = TestClock::default();
    let operator = spawn_operator(env, &ns, None, &clock);
    let statefulsets: Api<StatefulSet> = Api::namespaced(client.clone(), &ns);
    wait_for("the StatefulSet", Duration::from_secs(60), || async {
        statefulsets.get_opt("balerix-default").await.unwrap()
    })
    .await;
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &ns);
    let tls = secrets.get("balerix-default-tls").await.unwrap();
    let old_crt = tls.data.as_ref().unwrap()["tls.crt"].clone();
    let names = pki::daemon_names(&ns, "default");

    // a day on, so the reissued certificate's expiry, and the template, differ
    clock.advance(86_400);
    let now = clock.clock()();
    let other = pki::new_authority(&ns, "default", now).unwrap();
    let replaced = Patch::Merge(serde_json::json!({
        "metadata": { "annotations": { "balerix.ai/not-after": other.not_after.to_string() } },
        "stringData": { "ca.crt": other.cert_pem, "ca.key": other.key_pem }
    }));
    let text =
        |s: &Secret, key: &str| String::from_utf8(s.data.as_ref().unwrap()[key].0.clone()).unwrap();
    // a reconcile that read the authority before the patch applies the
    // old one back: patch again until the new one stays
    let reissued = wait_for(
        "a serving certificate from the new authority",
        Duration::from_secs(60),
        || async {
            let ca = secrets.get("balerix-default-ca").await.unwrap();
            if text(&ca, "ca.crt") != other.cert_pem {
                secrets
                    .patch("balerix-default-ca", &PatchParams::default(), &replaced)
                    .await
                    .unwrap();
                return None;
            }
            let s = secrets.get("balerix-default-tls").await.unwrap();
            (s.data.as_ref().unwrap()["tls.crt"] != old_crt
                && pki::verifies(&other.cert_pem, &text(&s, "tls.crt"), &names[0], now))
            .then_some(s)
        },
    )
    .await;
    // the roll follows the certificate, not its expiry: a reissue in the
    // same second as its predecessor shares its not-after (#142)
    assert_ne!(cert_of(&reissued), cert_of(&tls));
    wait_for(
        "the pod template to roll",
        Duration::from_secs(60),
        || async {
            let s = statefulsets.get("balerix-default").await.unwrap();
            (template_cert(s) == cert_of(&reissued)).then_some(())
        },
    )
    .await;
    // the authority the dropped reconcile left is kept
    assert_eq!(
        text(&secrets.get("balerix-default-ca").await.unwrap(), "ca.crt"),
        other.cert_pem
    );
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ca_secret_whose_key_is_not_its_certificates_is_reminted_once() {
    let Some(env) = envtest().await else { return };
    let ns = namespace(&env.client, "camismatch").await;
    let client = env.client.clone();
    Api::<Daemon>::namespaced(client.clone(), &ns)
        .create(
            &PostParams::default(),
            &Daemon::new("default", daemon_spec()),
        )
        .await
        .unwrap();
    let clock = TestClock::default();
    let operator = spawn_operator(env, &ns, None, &clock);
    let statefulsets: Api<StatefulSet> = Api::namespaced(client.clone(), &ns);
    wait_for("the StatefulSet", Duration::from_secs(60), || async {
        statefulsets.get_opt("balerix-default").await.unwrap()
    })
    .await;
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &ns);
    let text =
        |s: &Secret, key: &str| String::from_utf8(s.data.as_ref().unwrap()[key].0.clone()).unwrap();
    let ca = secrets.get("balerix-default-ca").await.unwrap();
    let (old_crt, old_key) = (text(&ca, "ca.crt"), text(&ca, "ca.key"));

    // a day on, so a reissued serving certificate's expiry differs
    clock.advance(86_400);
    let other = pki::new_authority(&ns, "default", clock.clock()()).unwrap();
    // the certificate kept, the key another authority's
    let broken = Patch::Merge(serde_json::json!({ "stringData": { "ca.key": other.key_pem } }));
    // a reconcile that read the old pair before the patch applies it back:
    // patch again while the old key is there
    let reminted = wait_for("a new authority", Duration::from_secs(60), || async {
        let ca = secrets.get("balerix-default-ca").await.unwrap();
        if text(&ca, "ca.crt") != old_crt {
            return Some(ca);
        }
        if text(&ca, "ca.key") == old_key {
            secrets
                .patch("balerix-default-ca", &PatchParams::default(), &broken)
                .await
                .unwrap();
        }
        None
    })
    .await;
    let new_crt = text(&reminted, "ca.crt");
    assert_ne!(new_crt, other.cert_pem);
    let now = clock.clock()();
    let names = pki::daemon_names(&ns, "default");
    let cert = wait_for(
        "a serving certificate from the new authority in the pod template",
        Duration::from_secs(60),
        || async {
            let tls = secrets.get("balerix-default-tls").await.unwrap();
            if !pki::verifies(&new_crt, &text(&tls, "tls.crt"), &names[0], now) {
                return None;
            }
            let cert = cert_of(&tls);
            let template = statefulsets.get("balerix-default").await.unwrap();
            (template_cert(template) == cert).then_some(cert)
        },
    )
    .await;
    // once: the authority and the pod template hold across reconciles (period 1 s)
    hold_for(
        "a second re-mint or roll",
        Duration::from_secs(5),
        || async {
            let ca = secrets.get("balerix-default-ca").await.unwrap();
            let template = statefulsets.get("balerix-default").await.unwrap();
            let rolled = template_cert(template) != cert;
            (text(&ca, "ca.crt") != new_crt || rolled).then_some(())
        },
    )
    .await;
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
    let first = wait_for("the pool Job", Duration::from_secs(60), || async {
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
    wait_for("the retry", Duration::from_secs(60), || async {
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
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_whose_pod_was_never_created_fails_at_its_deadline_and_is_retried() {
    let Some(env) = envtest().await else { return };
    let ns = namespace(&env.client, "expired").await;
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
    let first = wait_for("the pool Job", Duration::from_secs(60), || async {
        jobs.get_opt("balerix-default-pool").await.unwrap()
    })
    .await;

    // a quota refused its pod; the deadline failed it with `failed: 0`
    expire_job(&client, &ns, "balerix-default-pool").await;
    let status = wait_for(
        "SystemToolsReady=False for the expired Job",
        Duration::from_secs(10),
        || async {
            let s = daemons.get("default").await.unwrap().status?;
            let tools = condition(&s.conditions, "SystemToolsReady");
            (tools.status == "False" && tools.reason == "PoolSyncFailed").then_some(s)
        },
    )
    .await;
    assert_eq!(
        condition(&status.conditions, "SystemToolsReady").message,
        "the job failed: DeadlineExceeded: Job was active longer than specified deadline"
    );

    clock.advance(31);
    wait_for("the retry", Duration::from_secs(60), || async {
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
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_that_fails_readyz_keeps_its_not_ready_transition_time() {
    let Some(env) = envtest().await else { return };
    let ns = namespace(&env.client, "readyz").await;
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
    // a closed port: every `/readyz` fails
    let operator = spawn_operator(env, &ns, Some("http://127.0.0.1:1".into()), &clock);
    // the kubelet: the pool synced, the claim bound, the pod ready
    make_daemon_ready(&client, &ns).await;

    let ready = wait_for(
        "Ready=False/DaemonNotReady",
        Duration::from_secs(60),
        || async {
            let s = daemons.get("default").await.unwrap().status?;
            let ready = condition(&s.conditions, "Ready").clone();
            (ready.status == "False"
                && ready.reason == "DaemonNotReady"
                && ready.message != "the daemon pod is not ready")
                .then_some(ready)
        },
    )
    .await;
    hold_for(
        "Ready's transition time to move",
        Duration::from_secs(3),
        || async {
            let s = daemons.get("default").await.unwrap().status?;
            let now = condition(&s.conditions, "Ready").clone();
            (now.status != ready.status
                || now.reason != ready.reason
                || now.last_transition_time != ready.last_transition_time)
                .then_some(())
        },
    )
    .await;
    operator.abort();
}

fn fleet_spec(daemon: &str, crews: &[(&str, &[&str])], retain: &str) -> FleetSpec {
    let crews: serde_json::Map<String, serde_json::Value> = crews
        .iter()
        .map(|(crew, agents)| {
            let agents: serde_json::Map<String, serde_json::Value> =
                agents.iter().map(|a| (a.to_string(), serde_json::json!({}))).collect();
            (crew.to_string(), serde_json::json!({ "repo": "acme/api", "git": { "auth": "none" }, "agents": agents }))
        })
        .collect();
    serde_json::from_value(
        serde_json::json!({ "daemon": daemon, "retain": retain, "crews": crews }),
    )
    .unwrap()
}

/// A Daemon, a stub, an operator over a fresh namespace.
async fn world(
    label: &str,
) -> (
    &'static support::envtest::EnvTest,
    String,
    StubDaemon,
    TestClock,
    tokio::task::AbortHandle,
) {
    let env = envtest().await.expect("envtest");
    let ns = namespace(&env.client, label).await;
    Api::<Daemon>::namespaced(env.client.clone(), &ns)
        .create(
            &PostParams::default(),
            &Daemon::new("default", daemon_spec()),
        )
        .await
        .unwrap();
    let stub = StubDaemon::start().await;
    let clock = TestClock::default();
    let operator = spawn_operator(env, &ns, Some(stub.url()), &clock);
    (env, ns, stub, clock, operator)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fleet_becomes_crews_agents_and_tokens_and_the_put_carries_them() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, stub, _clock, operator) = world("fleet").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new(
                "f",
                fleet_spec("default", &[("c", &["a", "b"])], "Branches"),
            ),
        )
        .await
        .unwrap();

    let crews: Api<Crew> = Api::namespaced(client.clone(), &ns);
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    wait_for("the Crew", Duration::from_secs(60), || async {
        crews.get_opt("f-c").await.unwrap()
    })
    .await;
    let a = wait_for("Agent f-c-a", Duration::from_secs(10), || async {
        agents.get_opt("f-c-a").await.unwrap()
    })
    .await;
    wait_for("Agent f-c-b", Duration::from_secs(10), || async {
        agents.get_opt("f-c-b").await.unwrap()
    })
    .await;
    assert_eq!(
        a.metadata.finalizers.as_deref(),
        Some(&["balerix.ai/harvest".to_string()][..])
    );
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &ns);
    let token = secrets.get("f-c-a-token").await.unwrap();
    assert_eq!(token.data.as_ref().unwrap()["token"].0.len(), 64);
    assert_eq!(
        token.metadata.owner_references.as_ref().unwrap()[0].kind,
        "Fleet"
    );
    Api::<Job>::namespaced(client.clone(), &ns)
        .get("f-pool")
        .await
        .unwrap();

    let puts = stub.puts();
    assert!(!puts.is_empty());
    let tokens = puts[0].agent_tokens.as_ref().unwrap();
    assert_eq!(tokens.len(), 2);
    assert_eq!(
        String::from_utf8(token.data.as_ref().unwrap()["token"].0.clone()).unwrap(),
        tokens["f/c/a"]
    );
    assert_eq!(puts[0].spec.crews["c"].agents.len(), 2);

    let status = wait_for("Fleet status", Duration::from_secs(10), || async {
        fleets
            .get("f")
            .await
            .unwrap()
            .status
            // a reconcile before the Daemon's authority and admin Secret
            // exist writes all three with Accepted=Unknown (DaemonUnavailable)
            .filter(|s| {
                s.conditions.len() == 3 && condition(&s.conditions, "Accepted").status == "True"
            })
    })
    .await;
    assert_eq!(condition(&status.conditions, "Resolved").status, "True");
    assert_eq!(condition(&status.conditions, "Accepted").status, "True");
    let ready = condition(&status.conditions, "Ready");
    assert_eq!(
        (
            ready.status.as_str(),
            ready.reason.as_str(),
            ready.message.as_str()
        ),
        ("False", "AgentsNotReady", "0 of 2 agents ready")
    );
    assert_eq!(
        fleets
            .get("f")
            .await
            .unwrap()
            .metadata
            .finalizers
            .as_deref(),
        Some(&["balerix.ai/fleet".to_string()][..])
    );
    // the token is kept across reconciles
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(secrets.get("f-c-a-token").await.unwrap().data, token.data);
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_fleet_lands_no_child() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, stub, _clock, operator) = world("rejected").await;
    stub.reject(Some("crews.c.agents.a.model: no such model"));
    let fleets: Api<Fleet> = Api::namespaced(env.client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "Branches")),
        )
        .await
        .unwrap();
    let status = wait_for("Accepted=False", Duration::from_secs(60), || async {
        let s = fleets.get("f").await.unwrap().status?;
        (s.conditions.len() == 3 && condition(&s.conditions, "Accepted").status == "False")
            .then_some(s)
    })
    .await;
    let accepted = condition(&status.conditions, "Accepted");
    assert_eq!(
        (accepted.reason.as_str(), accepted.message.as_str()),
        ("Rejected", "crews.c.agents.a.model: no such model")
    );
    assert_eq!(condition(&status.conditions, "Ready").reason, "Rejected");
    let crews: Api<Crew> = Api::namespaced(env.client.clone(), &ns);
    hold_for(
        "a Crew of a rejected Fleet",
        Duration::from_secs(3),
        || async { crews.get_opt("f-c").await.unwrap() },
    )
    .await;
    assert!(
        Api::<Job>::namespaced(env.client.clone(), &ns)
            .get_opt("f-pool")
            .await
            .unwrap()
            .is_none()
    );
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_that_does_not_answer_leaves_the_children() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, stub, _clock, operator) = world("unavailable").await;
    let fleets: Api<Fleet> = Api::namespaced(env.client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "Branches")),
        )
        .await
        .unwrap();
    let crews: Api<Crew> = Api::namespaced(env.client.clone(), &ns);
    wait_for("the Crew", Duration::from_secs(60), || async {
        crews.get_opt("f-c").await.unwrap()
    })
    .await;
    // accepted first: a `DaemonUnavailable` from before the Daemon's
    // authority existed would otherwise satisfy the wait below
    wait_for("Accepted=True", Duration::from_secs(60), || async {
        let s = fleets.get("f").await.unwrap().status?;
        (s.conditions.len() == 3 && condition(&s.conditions, "Accepted").status == "True")
            .then_some(())
    })
    .await;
    stub.stop().await;
    let status = wait_for("DaemonUnavailable", Duration::from_secs(60), || async {
        let s = fleets.get("f").await.unwrap().status?;
        (condition(&s.conditions, "Ready").reason == "DaemonUnavailable").then_some(s)
    })
    .await;
    assert_eq!(condition(&status.conditions, "Accepted").status, "Unknown");
    crews.get("f-c").await.unwrap();
    Api::<Agent>::namespaced(env.client.clone(), &ns)
        .get("f-c-a")
        .await
        .unwrap();
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fleet_naming_no_daemon_is_not_resolved() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, stub, _clock, operator) = world("ghost").await;
    let fleets: Api<Fleet> = Api::namespaced(env.client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new("f", fleet_spec("ghost", &[("c", &["a"])], "Branches")),
        )
        .await
        .unwrap();
    let status = wait_for("Resolved=False", Duration::from_secs(60), || async {
        let s = fleets.get("f").await.unwrap().status?;
        (!s.conditions.is_empty()).then_some(s)
    })
    .await;
    let resolved = condition(&status.conditions, "Resolved");
    assert_eq!(
        (resolved.status.as_str(), resolved.message.as_str()),
        ("False", "spec.daemon: Daemon ghost does not exist")
    );
    assert!(stub.puts().is_empty());
    assert!(
        Api::<Crew>::namespaced(env.client.clone(), &ns)
            .get_opt("f-c")
            .await
            .unwrap()
            .is_none()
    );
    operator.abort();
}

/// #187: a Fleet whose `sandbox` block is off the fleet allowlist is
/// refused up front by the same check the Daemon runs, as the Fleet's
/// `Resolved=False` naming the path; nothing is sent or made.
#[tokio::test(flavor = "multi_thread")]
async fn a_fleet_with_a_refused_sandbox_key_is_not_resolved() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, stub, _clock, operator) = world("sandbox").await;
    let fleets: Api<Fleet> = Api::namespaced(env.client.clone(), &ns);
    let spec: FleetSpec = serde_json::from_value(serde_json::json!({
        "daemon": "default",
        "retain": "Branches",
        "crews": { "c": {
            "repo": "acme/api",
            "git": { "auth": "none" },
            "agents": { "a": { "sandbox": { "network": { "tls_intercept": {} } } } },
        } },
    }))
    .unwrap();
    fleets
        .create(&PostParams::default(), &Fleet::new("f", spec))
        .await
        .unwrap();
    let status = wait_for("Resolved=False", Duration::from_secs(60), || async {
        let s = fleets.get("f").await.unwrap().status?;
        (!s.conditions.is_empty()).then_some(s)
    })
    .await;
    let resolved = condition(&status.conditions, "Resolved");
    assert_eq!(
        (
            resolved.status.as_str(),
            resolved.reason.as_str(),
            resolved.message.as_str()
        ),
        (
            "False",
            "InvalidFleet",
            "crews.c.agents.a.sandbox.network.tls_intercept: not accepted in a fleet's sandbox block"
        )
    );
    assert!(stub.puts().is_empty());
    assert!(
        Api::<Crew>::namespaced(env.client.clone(), &ns)
            .get_opt("f-c")
            .await
            .unwrap()
            .is_none()
    );
    operator.abort();
}

/// #235: the definition's CEL rule refuses a change of `spec.daemon`;
/// other changes, and an update that leaves it as it is, pass.
#[tokio::test(flavor = "multi_thread")]
async fn a_fleets_daemon_cannot_be_changed() {
    if envtest().await.is_none() {
        return;
    }
    let env = envtest().await.unwrap();
    let ns = namespace(&env.client, "immutable").await;
    let fleets: Api<Fleet> = Api::namespaced(env.client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "Branches")),
        )
        .await
        .unwrap();

    let moved = fleets
        .patch(
            "f",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "daemon": "other" } })),
        )
        .await;
    match moved {
        Err(kube::Error::Api(e)) => {
            assert_eq!(e.code, 422, "{e:?}");
            assert!(
                e.message.contains("spec.daemon is immutable"),
                "{}",
                e.message
            );
        }
        other => panic!("the move was not refused: {other:?}"),
    }
    assert_eq!(fleets.get("f").await.unwrap().spec.daemon, "default");

    // the rest of the spec still changes, with the daemon restated or not
    fleets
        .patch(
            "f",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "daemon": "default", "retain": "None" } })),
        )
        .await
        .unwrap();
    let f = fleets
        .patch(
            "f",
            &PatchParams::default(),
            &Patch::Merge(
                serde_json::json!({ "spec": { "crews": { "c": { "agents": { "b": {} } } } } }),
            ),
        )
        .await
        .unwrap();
    assert_eq!(f.spec.daemon, "default");
    assert_eq!(f.spec.retain, balerix_operator::api::Retain::None);
    assert!(f.spec.crews["c"].agents.contains_key("b"));
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_namespaces_ignores_another_namespace() {
    if envtest().await.is_none() {
        return;
    }
    let (env, _ns, _stub, _clock, operator) = world("watched").await;
    // the operator watches `_ns`; this Daemon is elsewhere
    let other = namespace(&env.client, "unwatched").await;
    Api::<Daemon>::namespaced(env.client.clone(), &other)
        .create(
            &PostParams::default(),
            &Daemon::new("default", daemon_spec()),
        )
        .await
        .unwrap();
    let sts: Api<StatefulSet> = Api::namespaced(env.client.clone(), &other);
    hold_for(
        "a StatefulSet in an unwatched namespace",
        Duration::from_secs(4),
        || async { sts.get_opt("balerix-default").await.unwrap() },
    )
    .await;
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_sync_is_reported_and_retried_as_attempt_two() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, _stub, clock, operator) = world("sync").await;
    let client = env.client.clone();
    Api::<Fleet>::namespaced(client.clone(), &ns)
        .create(
            &PostParams::default(),
            &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "Branches")),
        )
        .await
        .unwrap();
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let crews: Api<Crew> = Api::namespaced(client.clone(), &ns);
    let first = wait_for("the sync Job", Duration::from_secs(60), || async {
        jobs.get_opt("f-c-sync").await.unwrap()
    })
    .await;
    let args = first
        .spec
        .as_ref()
        .unwrap()
        .template
        .spec
        .as_ref()
        .unwrap()
        .containers[0]
        .args
        .clone()
        .unwrap();
    assert_eq!(
        &args[..3],
        &[
            "crew-sync".to_string(),
            "--crew".to_string(),
            "f/c".to_string()
        ]
    );
    // the fleet pool succeeded; the sync failed on the cache
    finish_job(&client, &ns, "f-pool", true, Some("synced")).await;
    finish_job(
        &client,
        &ns,
        "f-c-sync",
        false,
        Some("cache: f/c: the remote has no branch nope"),
    )
    .await;
    let status = wait_for("CacheReady=SyncFailed", Duration::from_secs(10), || async {
        let s = crews.get("f-c").await.unwrap().status?;
        // False/Syncing comes first, while the Job runs
        (condition(&s.conditions, "CacheReady").reason == "SyncFailed").then_some(s)
    })
    .await;
    let cache = condition(&status.conditions, "CacheReady");
    assert_eq!(
        (cache.reason.as_str(), cache.message.as_str()),
        ("SyncFailed", "cache: f/c: the remote has no branch nope")
    );

    clock.advance(31);
    wait_for("the retry", Duration::from_secs(60), || async {
        let j = jobs.get_opt("f-c-sync").await.unwrap()?;
        (j.metadata.uid != first.metadata.uid).then_some(j)
    })
    .await;
    assert_eq!(
        crews
            .get("f-c")
            .await
            .unwrap()
            .metadata
            .annotations
            .unwrap()["balerix.ai/attempts"],
        "{\"f-c-sync\":2}"
    );
    // the second run succeeds: both conditions true, cacheRef the commit
    finish_job(&client, &ns, "f-c-sync", true, Some("0123abcd")).await;
    let status = wait_for("CacheReady=True", Duration::from_secs(10), || async {
        let s = crews.get("f-c").await.unwrap().status?;
        (condition(&s.conditions, "CacheReady").status == "True"
            && condition(&s.conditions, "ToolsReady").status == "True"
            // the Job's status lands before its pod's message
            && s.cache_ref.is_some())
        .then_some(s)
    })
    .await;
    assert_eq!(status.cache_ref.as_deref(), Some("0123abcd"));
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn two_fleets_sharing_a_crew_name_sync_at_once() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, _stub, _clock, operator) = world("twocrews").await;
    let fleets: Api<Fleet> = Api::namespaced(env.client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new(
                "payments",
                fleet_spec("default", &[("backend", &["a"])], "Branches"),
            ),
        )
        .await
        .unwrap();
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new(
                "billing",
                fleet_spec("default", &[("backend", &["a"])], "Branches"),
            ),
        )
        .await
        .unwrap();
    let jobs: Api<Job> = Api::namespaced(env.client.clone(), &ns);
    wait_for("both sync Jobs", Duration::from_secs(60), || async {
        let p = jobs.get_opt("payments-backend-sync").await.unwrap()?;
        let b = jobs.get_opt("billing-backend-sync").await.unwrap()?;
        Some((p, b))
    })
    .await;
    operator.abort();
}

/// Both Jobs of crew `c` succeeded: its Agents may have pods.
async fn crew_ready(client: &kube::Client, ns: &str, fleet: &str, crew: &str) {
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);
    let pool = format!("{fleet}-pool");
    let sync = format!("{fleet}-{crew}-sync");
    wait_for("the Jobs", Duration::from_secs(60), || async {
        jobs.get_opt(&pool).await.unwrap()?;
        jobs.get_opt(&sync).await.unwrap()
    })
    .await;
    // the pool is the Fleet's: a second crew finds it finished, and a
    // finished Job's status is immutable
    let pool_done = jobs
        .get(&pool)
        .await
        .unwrap()
        .status
        .and_then(|s| s.succeeded)
        .is_some_and(|n| n > 0);
    if !pool_done {
        finish_job(client, ns, &pool, true, Some("synced")).await;
    }
    finish_job(client, ns, &sync, true, Some("0123abcd")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crew_dropped_from_the_fleet_is_removed_and_its_agent_harvested() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, _stub, _clock, operator) = world("dropped").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new(
                "f",
                fleet_spec("default", &[("c", &["a"]), ("d", &["a"])], "Branches"),
            ),
        )
        .await
        .unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    crew_ready(&client, &ns, "f", "d").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
    wait_for("the pods", Duration::from_secs(60), || async {
        pods.get_opt("f-c-a").await.unwrap()?;
        pods.get_opt("f-d-a").await.unwrap()
    })
    .await;
    claims.get("f-d-a").await.unwrap();
    Api::<Secret>::namespaced(client.clone(), &ns)
        .get("f-d-a-bundle")
        .await
        .unwrap();

    // crew d leaves the Fleet
    fleets
        .patch(
            "f",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "crews": { "d": null } } })),
        )
        .await
        .unwrap();
    reap_pod(&client, &ns, "f-d-a", Duration::from_secs(60)).await;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    wait_for("the harvest Job", Duration::from_secs(60), || async {
        jobs.get_opt("f-d-a-harvest").await.unwrap()
    })
    .await;
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    // the Agent says what its deletion waits for, and how to skip it
    let ready = wait_for("Ready=False/Deleting", Duration::from_secs(60), || async {
        let s = agents.get("f-d-a").await.unwrap().status?;
        let c = condition(&s.conditions, "Ready").clone();
        (c.reason == "Deleting" && c.message.contains("harvest")).then_some(c)
    })
    .await;
    assert_eq!(ready.status, "False");
    assert_eq!(
        ready.message,
        format!(
            "Agent {ns}/f-d-a: harvest Job f-d-a-harvest not finished; the annotation balerix.ai/purge=true on the Agent skips its harvest; on the Fleet, while the Fleet is deleted, it skips every harvest and also purges the Fleet's record on the Daemon"
        )
    );
    finish_job(
        &client,
        &ns,
        "f-d-a-harvest",
        true,
        Some("harvested balerix/a"),
    )
    .await;
    wait_for("Agent f-d-a gone", Duration::from_secs(60), || async {
        agents
            .get_opt("f-d-a")
            .await
            .unwrap()
            .is_none()
            .then_some(())
    })
    .await;
    // the controller-manager: the claim's protection comes off
    reap_claim(&client, &ns, "f-d-a", Duration::from_secs(10)).await;
    wait_for("Crew f-d gone", Duration::from_secs(10), || async {
        Api::<Crew>::namespaced(client.clone(), &ns)
            .get_opt("f-d")
            .await
            .unwrap()
            .is_none()
            .then_some(())
    })
    .await;
    agents.get("f-c-a").await.unwrap();
    pods.get("f-c-a").await.unwrap();
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_with_no_claim_is_removed_without_a_harvest() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, _stub, _clock, operator) = world("noclaim").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new(
                "f",
                fleet_spec("default", &[("c", &["a"]), ("d", &["a"])], "Branches"),
            ),
        )
        .await
        .unwrap();
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    wait_for("Agent f-d-a", Duration::from_secs(60), || async {
        agents.get_opt("f-d-a").await.unwrap()
    })
    .await;
    let status = wait_for("WaitingForCrew", Duration::from_secs(10), || async {
        let s = agents.get("f-d-a").await.unwrap().status?;
        (condition(&s.conditions, "Materialized").reason == "WaitingForCrew").then_some(s)
    })
    .await;
    assert_eq!(
        condition(&status.conditions, "Materialized").status,
        "Unknown"
    );
    // dropped before its crew ever synced: no claim, no harvest
    fleets
        .patch(
            "f",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "crews": { "d": null } } })),
        )
        .await
        .unwrap();
    wait_for("Agent f-d-a gone", Duration::from_secs(60), || async {
        agents
            .get_opt("f-d-a")
            .await
            .unwrap()
            .is_none()
            .then_some(())
    })
    .await;
    assert!(
        Api::<Job>::namespaced(client.clone(), &ns)
            .get_opt("f-d-a-harvest")
            .await
            .unwrap()
            .is_none()
    );
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_spec_hash_replaces_the_pod_and_keeps_the_claim() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, _stub, _clock, operator) = world("spechash").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "Branches")),
        )
        .await
        .unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let first = wait_for("the pod", Duration::from_secs(60), || async {
        pods.get_opt("f-c-a").await.unwrap()
    })
    .await;
    let claim = Api::<PersistentVolumeClaim>::namespaced(client.clone(), &ns)
        .get("f-c-a")
        .await
        .unwrap();
    let old_hash = first.metadata.annotations.as_ref().unwrap()["balerix.ai/spec-hash"].clone();

    fleets.patch("f", &PatchParams::default(), &Patch::Merge(serde_json::json!({
        "spec": { "crews": { "c": { "agents": { "a": { "claude": { "settings": { "model": "opus" } } } } } } }
    }))).await.unwrap();
    reap_pod_uid(
        &client,
        &ns,
        "f-c-a",
        first.metadata.uid.as_deref().unwrap(),
        Duration::from_secs(60),
    )
    .await;
    let second = wait_for("the new pod", Duration::from_secs(60), || async {
        let p = pods.get_opt("f-c-a").await.unwrap()?;
        (p.metadata.uid != first.metadata.uid).then_some(p)
    })
    .await;
    let new_hash = &second.metadata.annotations.as_ref().unwrap()["balerix.ai/spec-hash"];
    assert_ne!(*new_hash, old_hash);
    assert_eq!(
        Api::<Agent>::namespaced(client.clone(), &ns)
            .get("f-c-a")
            .await
            .unwrap()
            .spec
            .spec_hash,
        *new_hash
    );
    assert_eq!(
        Api::<PersistentVolumeClaim>::namespaced(client.clone(), &ns)
            .get("f-c-a")
            .await
            .unwrap()
            .metadata
            .uid,
        claim.metadata.uid
    );
    operator.abort();
}

/// §11, #234: the kubelet's node-pressure eviction leaves the pod object
/// in phase `Failed`; its specHash still matches, and it is replaced on
/// the same claim all the same. Pods evicted young wait before their
/// replacement, longer for each in a row, and each replacement is a
/// `PodReplaced` Event on the Agent.
#[tokio::test(flavor = "multi_thread")]
async fn an_evicted_pod_is_replaced_on_the_same_claim_backing_off() {
    use balerix_operator::desired::agent::{POD_BACKOFF_ANNOTATION, PodBackoff};
    use k8s_openapi::api::events::v1::Event;
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, _stub, clock, operator) = world("evicted").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "Branches")),
        )
        .await
        .unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    let events: Api<Event> = Api::namespaced(client.clone(), &ns);
    let mut pod = wait_for("the pod", Duration::from_secs(60), || async {
        pods.get_opt("f-c-a").await.unwrap()
    })
    .await;
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
    let claim = claims.get("f-c-a").await.unwrap();

    for (streak, delay) in [(1u32, 10u64), (2, 20)] {
        let uid = pod.metadata.uid.clone().unwrap();
        pods.patch_status(
            "f-c-a",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "status": {
                "phase": "Failed", "reason": "Evicted",
                "message": "The node was low on resource: memory."
            } })),
        )
        .await
        .unwrap();
        // the pod lived seconds: its replacement waits, longer each time
        let backoff = wait_for(
            "the back-off on the Agent",
            Duration::from_secs(60),
            || async {
                let a = agents.get("f-c-a").await.unwrap();
                let b: PodBackoff =
                    serde_json::from_str(a.metadata.annotations?.get(POD_BACKOFF_ANNOTATION)?)
                        .ok()?;
                (b.pod == uid).then_some(b)
            },
        )
        .await;
        assert_eq!((backoff.streak, backoff.delay), (streak, delay));
        hold_for("the ended pod waits", Duration::from_secs(3), || async {
            let p = pods.get_opt("f-c-a").await.unwrap();
            (p?.metadata.uid.as_deref() != Some(uid.as_str())).then_some(())
        })
        .await;

        clock.advance(delay as i64);
        reap_pod_uid(&client, &ns, "f-c-a", &uid, Duration::from_secs(60)).await;
        let next = wait_for("the new pod", Duration::from_secs(60), || async {
            let p = pods.get_opt("f-c-a").await.unwrap()?;
            (p.metadata.uid.as_deref() != Some(uid.as_str())).then_some(p)
        })
        .await;
        assert_eq!(
            next.metadata.annotations.as_ref().unwrap()["balerix.ai/spec-hash"],
            pod.metadata.annotations.as_ref().unwrap()["balerix.ai/spec-hash"]
        );
        pod = next;
    }
    assert_eq!(
        claims.get("f-c-a").await.unwrap().metadata.uid,
        claim.metadata.uid
    );
    // the recorder folds a repeat within its cache's lifetime into the
    // first Event's `series`, keeping the first note
    let event = wait_for(
        "a PodReplaced Event on the Agent, counted twice",
        Duration::from_secs(30),
        || async {
            events
                .list(&Default::default())
                .await
                .unwrap()
                .items
                .into_iter()
                .find(|e| {
                    e.reason.as_deref() == Some("PodReplaced")
                        && e.regarding.as_ref().and_then(|r| r.name.as_deref()) == Some("f-c-a")
                        && e.series.as_ref().is_some_and(|s| s.count == 2)
                })
        },
    )
    .await;
    assert_eq!(event.type_.as_deref(), Some("Warning"));
    assert_eq!(
        event.note.as_deref(),
        Some(
            "pod f-c-a ended in phase Failed (Evicted: The node was low on resource: memory.); replaced on the same claim after a 10 s back-off (1 in a row ended within 300 s of creation)"
        )
    );
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_crew_lock_holds_the_harvest_while_a_sync_runs() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, _stub, _clock, operator) = world("lock").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new(
                "f",
                fleet_spec("default", &[("c", &["a", "b"])], "Branches"),
            ),
        )
        .await
        .unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    wait_for("the pods", Duration::from_secs(60), || async {
        pods.get_opt("f-c-a").await.unwrap()?;
        pods.get_opt("f-c-b").await.unwrap()
    })
    .await;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let synced = jobs.get("f-c-sync").await.unwrap();

    // a changed crew tool table makes the sync Job stale: a new one runs, unfinished
    fleets.patch("f", &PatchParams::default(), &Patch::Merge(serde_json::json!({ "spec": { "crews": { "c": { "defaults": { "tools": { "node": "22.11.0" } } } } } }))).await.unwrap();
    // the garbage collector: the stale Job, deleted in the foreground, goes
    support::reap_job(
        &client,
        &ns,
        "f-c-sync",
        synced.metadata.uid.as_deref().unwrap(),
        Duration::from_secs(60),
    )
    .await;
    wait_for("a new sync Job", Duration::from_secs(60), || async {
        let j = jobs.get_opt("f-c-sync").await.unwrap()?;
        (j.metadata.uid != synced.metadata.uid).then_some(j)
    })
    .await;
    // agent b leaves while it runs
    fleets
        .patch(
            "f",
            &PatchParams::default(),
            &Patch::Merge(
                serde_json::json!({ "spec": { "crews": { "c": { "agents": { "b": null } } } } }),
            ),
        )
        .await
        .unwrap();
    reap_pod(&client, &ns, "f-c-b", Duration::from_secs(60)).await;
    hold_for(
        "a harvest while the sync runs",
        Duration::from_secs(4),
        || async { jobs.get_opt("f-c-b-harvest").await.unwrap() },
    )
    .await;
    // not vacuous: agent b is still there, deleting, with its claim
    let b = Api::<Agent>::namespaced(client.clone(), &ns)
        .get("f-c-b")
        .await
        .unwrap();
    assert!(b.metadata.deletion_timestamp.is_some());
    Api::<PersistentVolumeClaim>::namespaced(client.clone(), &ns)
        .get("f-c-b")
        .await
        .unwrap();
    finish_job(&client, &ns, "f-c-sync", true, Some("4567abcd")).await;
    wait_for(
        "the harvest after the sync",
        Duration::from_secs(60),
        || async { jobs.get_opt("f-c-b-harvest").await.unwrap() },
    )
    .await;
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn fleet_deletion_with_retain_none_downs_the_daemon_and_runs_the_cleanup_job() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, stub, clock, operator) = world("retainnone").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "None")),
        )
        .await
        .unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    wait_for("the pod", Duration::from_secs(60), || async {
        pods.get_opt("f-c-a").await.unwrap()
    })
    .await;

    // a finalizer of the test's own holds the Agent, so the Fleet's
    // deletion has something to wait on
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    let hold = |finalizers: serde_json::Value| {
        Patch::Merge(serde_json::json!({ "metadata": { "finalizers": finalizers } }))
    };
    agents
        .patch(
            "f-c-a",
            &PatchParams::default(),
            &hold(serde_json::json!(["balerix.ai/harvest", "test/hold"])),
        )
        .await
        .unwrap();
    fleets
        .delete("f", &kube::api::DeleteParams::default())
        .await
        .unwrap();
    // the deletion waits on the Agent
    let ready = wait_for("Ready=False/Deleting", Duration::from_secs(60), || async {
        let s = fleets.get("f").await.unwrap().status?;
        let c = condition(&s.conditions, "Ready").clone();
        (c.reason == "Deleting").then_some(c)
    })
    .await;
    assert_eq!(ready.status, "False");
    assert!(
        ready.message.contains("1 agents still exist (f-c-a)")
            && ready.message.contains("balerix.ai/purge=true"),
        "{}",
        ready.message
    );
    reap_pod(&client, &ns, "f-c-a", Duration::from_secs(60)).await;
    // the operator's own finalizer comes off first; then the test's
    wait_for(
        "the harvest finalizer off",
        Duration::from_secs(60),
        || async {
            let a = agents.get("f-c-a").await.unwrap();
            (a.metadata.finalizers.unwrap_or_default() == ["test/hold"]).then_some(())
        },
    )
    .await;
    agents
        .patch(
            "f-c-a",
            &PatchParams::default(),
            &hold(serde_json::json!(null)),
        )
        .await
        .unwrap();
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    wait_for(
        "Agent gone without a harvest",
        Duration::from_secs(60),
        || async {
            agents
                .get_opt("f-c-a")
                .await
                .unwrap()
                .is_none()
                .then_some(())
        },
    )
    .await;
    assert!(jobs.get_opt("f-c-a-harvest").await.unwrap().is_none());
    wait_for("the Daemon's DELETE", Duration::from_secs(60), || async {
        stub.deletes().contains(&"f".to_string()).then_some(())
    })
    .await;
    wait_for("the cleanup Job", Duration::from_secs(60), || async {
        jobs.get_opt("f-c-remove").await.unwrap()
    })
    .await;
    assert!(
        fleets.get_opt("f").await.unwrap().is_some(),
        "the finalizer waits for the cleanup"
    );
    // a failed cleanup says so on the Fleet, and is retried after the delay
    let first = jobs.get("f-c-remove").await.unwrap();
    finish_job(
        &client,
        &ns,
        "f-c-remove",
        false,
        Some("cannot empty the slice at /balerix/shared"),
    )
    .await;
    wait_for(
        "Deleting with the cleanup's failure",
        Duration::from_secs(60),
        || async {
            let s = fleets.get("f").await.unwrap().status?;
            let c = condition(&s.conditions, "Ready").clone();
            (c.reason == "Deleting"
                && c.message.contains(
                    "c (Job f-c-remove failed: cannot empty the slice at /balerix/shared)",
                ))
            .then_some(())
        },
    )
    .await;
    clock.advance(31);
    wait_for("the cleanup retried", Duration::from_secs(60), || async {
        let j = jobs.get_opt("f-c-remove").await.unwrap()?;
        (j.metadata.uid != first.metadata.uid).then_some(())
    })
    .await;
    finish_job(&client, &ns, "f-c-remove", true, Some("removed")).await;
    wait_for("the Fleet gone", Duration::from_secs(60), || async {
        fleets.get_opt("f").await.unwrap().is_none().then_some(())
    })
    .await;
    // O-16: unmarked, a plain down, which keeps the record
    assert!(
        stub.delete_queries().iter().all(|(_, q)| !q.purge),
        "{:?}",
        stub.delete_queries()
    );
    operator.abort();
}

/// A Fleet of one crew of one agent, annotated `balerix.ai/purge` with
/// `purge` (§25); its agent's pod made, then the Fleet deleted.
async fn delete_annotated_fleet(
    client: &kube::Client,
    ns: &str,
    retain: &str,
    purge: &str,
) -> Api<Fleet> {
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), ns);
    let mut fleet = Fleet::new("f", fleet_spec("default", &[("c", &["a"])], retain));
    fleet.metadata.annotations = Some(
        [("balerix.ai/purge".to_string(), purge.to_string())]
            .into_iter()
            .collect(),
    );
    fleets.create(&PostParams::default(), &fleet).await.unwrap();
    crew_ready(client, ns, "f", "c").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    wait_for("the pod", Duration::from_secs(60), || async {
        pods.get_opt("f-c-a").await.unwrap()
    })
    .await;
    fleets.delete("f", &DeleteParams::default()).await.unwrap();
    reap_pod(client, ns, "f-c-a", Duration::from_secs(60)).await;
    fleets
}

/// §25 (#135): a marked Fleet is downed, its crews cleaned up, and only
/// then purged on the Daemon, forced as the down is; the finalizer goes
/// after the purge.
#[tokio::test(flavor = "multi_thread")]
async fn a_purge_marked_fleet_is_purged_after_its_down_and_cleanup_job() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, stub, _clock, operator) = world("purgemarked").await;
    let client = env.client.clone();
    let fleets = delete_annotated_fleet(&client, &ns, "None", "true").await;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    wait_for("the cleanup Job", Duration::from_secs(60), || async {
        jobs.get_opt("f-c-remove").await.unwrap()
    })
    .await;
    let purged = || async {
        stub.delete_queries()
            .iter()
            .any(|(f, q)| f == "f" && q.purge)
            .then_some(())
    };
    // downed, and not purged while the cleanup runs
    assert!(
        stub.delete_queries()
            .iter()
            .any(|(f, q)| f == "f" && q.force && !q.purge),
        "{:?}",
        stub.delete_queries()
    );
    hold_for(
        "a purge before the cleanup finished",
        Duration::from_secs(3),
        purged,
    )
    .await;
    assert!(fleets.get_opt("f").await.unwrap().is_some());
    finish_job(&client, &ns, "f-c-remove", true, Some("removed")).await;
    wait_for("the Fleet gone", Duration::from_secs(60), || async {
        fleets.get_opt("f").await.unwrap().is_none().then_some(())
    })
    .await;
    let queries = stub.delete_queries();
    let first_purge = queries.iter().position(|(_, q)| q.purge).expect("a purge");
    assert!(
        queries[..first_purge].iter().all(|(_, q)| !q.purge) && first_purge > 0,
        "the down comes first: {queries:?}"
    );
    let (fleet, purge) = queries.last().unwrap();
    assert_eq!(fleet, "f");
    assert!(
        purge.purge && purge.force && !purge.keep_repos && !purge.keep_sessions,
        "{purge:?}"
    );
    operator.abort();
}

/// §25: a value other than `"true"` is no mark: the deletion is a plain
/// down (O-16), with no purge.
#[tokio::test(flavor = "multi_thread")]
async fn a_fleet_not_marked_true_is_downed_and_not_purged() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, stub, _clock, operator) = world("purgeunmarked").await;
    let client = env.client.clone();
    let fleets = delete_annotated_fleet(&client, &ns, "None", "yes").await;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    wait_for("the cleanup Job", Duration::from_secs(60), || async {
        jobs.get_opt("f-c-remove").await.unwrap()
    })
    .await;
    finish_job(&client, &ns, "f-c-remove", true, Some("removed")).await;
    wait_for("the Fleet gone", Duration::from_secs(60), || async {
        fleets.get_opt("f").await.unwrap().is_none().then_some(())
    })
    .await;
    let queries = stub.delete_queries();
    assert!(
        !queries.is_empty() && queries.iter().all(|(f, q)| f == "f" && q.force && !q.purge),
        "{queries:?}"
    );
    operator.abort();
}

/// §25: a purge the Daemon fails says so on the Fleet and holds it; a
/// Daemon with no record left (404) is purged already, and the finalizer
/// goes. `retain: Branches` with the mark runs no cleanup Job and no
/// harvest: the crew cache is kept, and only the record goes.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_purge_is_retried_and_a_404_lets_the_fleet_go() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, stub, _clock, operator) = world("purge404").await;
    // every purge fails until the test says otherwise
    stub.answer_purges(&[axum::http::StatusCode::INTERNAL_SERVER_ERROR]);
    let client = env.client.clone();
    let fleets = delete_annotated_fleet(&client, &ns, "Branches", "true").await;
    let ready = wait_for(
        "Deleting with the purge's failure",
        Duration::from_secs(60),
        || async {
            let s = fleets.get_opt("f").await.unwrap()?.status?;
            let c = condition(&s.conditions, "Ready").clone();
            (c.reason == "Deleting" && c.message.contains("did not take the purge")).then_some(c)
        },
    )
    .await;
    assert_eq!(ready.status, "False");
    assert_eq!(
        ready.message,
        "Fleet f: Daemon default did not take the purge: the Daemon answered 500: no purge of f (500 Internal Server Error); it is retried, and removing the annotation balerix.ai/purge lets the Fleet go with its record kept"
    );
    assert!(fleets.get_opt("f").await.unwrap().is_some());
    stub.answer_purges(&[axum::http::StatusCode::NOT_FOUND]);
    wait_for("the Fleet gone", Duration::from_secs(60), || async {
        fleets.get_opt("f").await.unwrap().is_none().then_some(())
    })
    .await;
    let purges = stub
        .delete_queries()
        .iter()
        .filter(|(_, q)| q.purge)
        .count();
    assert!(purges >= 2, "{:?}", stub.delete_queries());
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    assert!(jobs.get_opt("f-c-remove").await.unwrap().is_none());
    assert!(jobs.get_opt("f-c-a-harvest").await.unwrap().is_none());
    operator.abort();
}

/// §25.3: a Fleet marked from the start ("purge on delete", as a GitOps
/// manifest carries it) still harvests an Agent dropped during its life;
/// only its deletion skips the harvests, and then purges its record.
#[tokio::test(flavor = "multi_thread")]
async fn a_fleet_marked_from_the_start_harvests_until_it_is_deleted() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, stub, _clock, operator) = world("purgelive").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    let mut fleet = Fleet::new(
        "f",
        fleet_spec("default", &[("c", &["a", "b"])], "Branches"),
    );
    fleet.metadata.annotations = Some(
        [("balerix.ai/purge".to_string(), "true".to_string())]
            .into_iter()
            .collect(),
    );
    fleets.create(&PostParams::default(), &fleet).await.unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    for pod in ["f-c-a", "f-c-b"] {
        wait_for("the pods", Duration::from_secs(60), || async {
            pods.get_opt(pod).await.unwrap()
        })
        .await;
    }

    // b dropped from the live, marked Fleet: harvested all the same
    fleets
        .patch(
            "f",
            &PatchParams::default(),
            &Patch::Merge(
                serde_json::json!({ "spec": { "crews": { "c": { "agents": { "b": null } } } } }),
            ),
        )
        .await
        .unwrap();
    reap_pod(&client, &ns, "f-c-b", Duration::from_secs(60)).await;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    wait_for("b's harvest Job", Duration::from_secs(60), || async {
        jobs.get_opt("f-c-b-harvest").await.unwrap()
    })
    .await;
    finish_job(&client, &ns, "f-c-b-harvest", true, Some("harvested b")).await;
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    wait_for("Agent f-c-b gone", Duration::from_secs(60), || async {
        agents
            .get_opt("f-c-b")
            .await
            .unwrap()
            .is_none()
            .then_some(())
    })
    .await;
    reap_claim(&client, &ns, "f-c-b", Duration::from_secs(60)).await;
    assert!(
        stub.delete_queries().is_empty(),
        "{:?}",
        stub.delete_queries()
    );

    // the Fleet deleted: a's harvest skipped, the record purged
    fleets.delete("f", &DeleteParams::default()).await.unwrap();
    reap_pod(&client, &ns, "f-c-a", Duration::from_secs(60)).await;
    wait_for("the Fleet gone", Duration::from_secs(60), || async {
        fleets.get_opt("f").await.unwrap().is_none().then_some(())
    })
    .await;
    assert!(jobs.get_opt("f-c-a-harvest").await.unwrap().is_none());
    assert!(jobs.get_opt("f-c-remove").await.unwrap().is_none());
    let queries = stub.delete_queries();
    assert!(
        queries
            .last()
            .is_some_and(|(f, q)| f == "f" && q.purge && q.force),
        "{queries:?}"
    );
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_status_mirrors_the_daemons_record_and_readiness_counts() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, stub, _clock, operator) = world("mirror").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "Branches")),
        )
        .await
        .unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    wait_for("the pod", Duration::from_secs(60), || async {
        pods.get_opt("f-c-a").await.unwrap()
    })
    .await;
    stub.set_agent(
        "f",
        "f/c/a",
        DaemonAgentStatus {
            phase: AgentPhase::Ready,
            restarts: 2,
            ..Default::default()
        },
    );
    // the kubelet: the sidecar is ready
    pods.patch_status("f-c-a", &PatchParams::default(), &Patch::Merge(serde_json::json!({
        "status": { "phase": "Running", "conditions": [{ "type": "PodScheduled", "status": "True" }],
            "initContainerStatuses": [{ "name": "sidecar", "image": "x", "imageID": "x", "ready": true, "restartCount": 0, "state": { "running": { "startedAt": "2026-10-03T00:00:00Z" } } }] }
    }))).await.unwrap();
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    let status = wait_for(
        "phase ready and Ready=True",
        Duration::from_secs(60),
        || async {
            let s = agents.get("f-c-a").await.unwrap().status?;
            (s.phase.as_deref() == Some("ready")
                && s.conditions
                    .iter()
                    .any(|c| c.type_ == "Ready" && c.status == "True"))
            .then_some(s)
        },
    )
    .await;
    assert_eq!(status.restarts, Some(2));
    assert_eq!(status.pod.as_deref(), Some("f-c-a"));
    assert_eq!(condition(&status.conditions, "Ready").status, "True");
    assert_eq!(condition(&status.conditions, "Materialized").status, "True");
    let fleet = wait_for("Fleet Ready", Duration::from_secs(60), || async {
        let s = fleets.get("f").await.unwrap().status?;
        (condition(&s.conditions, "Ready").status == "True").then_some(s)
    })
    .await;
    assert_eq!(condition(&fleet.conditions, "Ready").reason, "AgentsReady");
    operator.abort();
}

/// A Job of crew `f/c` as `ensure_job` compares it: by name, labels and
/// input hash. The API server needs a template; nothing runs it.
fn crew_job(ns: &str, name: &str, input: &str) -> Job {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "batch/v1", "kind": "Job",
        "metadata": { "name": name, "namespace": ns,
            "labels": { "balerix.ai/fleet": "f", "balerix.ai/crew": "c" },
            "annotations": { "balerix.ai/input-hash": input } },
        "spec": { "backoffLimit": 0, "template": { "spec": {
            "restartPolicy": "Never", "containers": [{ "name": "job", "image": "x" }] } } }
    }))
    .unwrap()
}

/// A `Context` of its own and an owner for the attempts annotation: the
/// Job rule called directly, with no controller running.
async fn job_rule(
    label: &str,
) -> (
    &'static support::envtest::EnvTest,
    String,
    balerix_operator::controllers::Context,
    ConfigMap,
) {
    let env = envtest().await.expect("envtest");
    let ns = namespace(&env.client, label).await;
    let owner: ConfigMap = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "ConfigMap", "metadata": { "name": "owner", "namespace": ns }
    }))
    .unwrap();
    let owner = Api::<ConfigMap>::namespaced(env.client.clone(), &ns)
        .create(&PostParams::default(), &owner)
        .await
        .unwrap();
    let mut cfg = balerix_operator::controllers::RunConfig::new(
        "0.2.0",
        balerix_operator::desired::common::Images {
            daemon: "balerix:test".into(),
            agent: "balerix-agent:test".into(),
        },
        &ns,
    );
    cfg.period = Duration::from_secs(1);
    cfg.stuck_after = Duration::from_secs(1);
    let ctx = balerix_operator::controllers::Context::new(env.client.clone(), cfg);
    (env, ns, ctx, owner)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_crew_lock_lets_one_of_two_racing_jobs_start() {
    use balerix_operator::controllers::jobs::ensure_job;
    use balerix_operator::desired::common::JobOutcome;
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, ctx, owner) = job_rule("race").await;
    let jobs: Api<Job> = Api::namespaced(env.client.clone(), &ns);
    // a sync and a harvest of one crew, both seeing it idle at once: the
    // lock is held across the check and the create, so one of them waits.
    // Twenty crews, each raced once: without the lock some round starts both
    for round in 0..20 {
        let crew = format!("c{round}");
        let job = |name: &str, input: &str| {
            let mut j = crew_job(&ns, name, input);
            j.metadata
                .labels
                .as_mut()
                .unwrap()
                .insert("balerix.ai/crew".into(), crew.clone());
            j
        };
        let (sync_name, harvest_name) = (format!("f-{crew}-sync"), format!("f-{crew}-a-harvest"));
        let (sync, harvest) = tokio::join!(
            ensure_job(&ctx, &owner, job(&sync_name, "s"), Some(("f", &crew))),
            ensure_job(&ctx, &owner, job(&harvest_name, "h"), Some(("f", &crew))),
        );
        let (sync, harvest) = (sync.unwrap(), harvest.unwrap());
        let made: Vec<String> = jobs
            .list(&kube::api::ListParams::default().labels(&format!("balerix.ai/crew={crew}")))
            .await
            .unwrap()
            .items
            .iter()
            .map(|j| j.metadata.name.clone().unwrap())
            .collect();
        assert_eq!(made.len(), 1, "one Job of crew {crew} at a time: {made:?}");
        // the one that waited saw the crew busy: `Absent`, not created
        let (waited, other) = if made[0] == sync_name {
            (&harvest, job(&harvest_name, "h"))
        } else {
            (&sync, job(&sync_name, "s"))
        };
        assert_eq!(waited.outcome, JobOutcome::Absent);
        // and a later look still refuses while the first is unfinished
        let again = ensure_job(&ctx, &owner, other, Some(("f", &crew)))
            .await
            .unwrap();
        assert_eq!(again.outcome, JobOutcome::Absent);
    }
    assert_eq!(
        jobs.list(&Default::default()).await.unwrap().items.len(),
        20
    );
}

/// #236: two Jobs of one owner fail and come due in one reconcile, each
/// `ensure_job` given the same snapshot of the owner: each Job's count
/// advances, and an entry the snapshot does not have is kept.
#[tokio::test(flavor = "multi_thread")]
async fn two_jobs_of_one_owner_failing_at_once_each_advance_their_attempt() {
    use balerix_operator::controllers::jobs::{ATTEMPTS_ANNOTATION, ensure_job};
    use balerix_operator::desired::common::JobOutcome;
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, mut ctx, owner) = job_rule("attempts").await;
    // every failure is past its delay (at most 10 min)
    ctx.run.clock = std::sync::Arc::new(|| k8s_openapi::jiff::Timestamp::now().as_second() + 3600);
    let owners: Api<ConfigMap> = Api::namespaced(env.client.clone(), &ns);
    let attempts = || async {
        let o = owners.get("owner").await.unwrap();
        let value = o.metadata.annotations.unwrap_or_default()[ATTEMPTS_ANNOTATION].clone();
        serde_json::from_str::<std::collections::BTreeMap<String, u32>>(&value).unwrap()
    };
    let names = ["f-c1-sync", "f-c2-sync"];
    let job = |name: &str| crew_job(&ns, name, "i");
    let mut snapshot = owner;
    for round in 2..=3u32 {
        for name in names {
            let made = ensure_job(&ctx, &snapshot, job(name), None).await.unwrap();
            assert_eq!(made.outcome, JobOutcome::Running);
            finish_job(&env.client, &ns, name, false, Some("boom")).await;
        }
        if round == 2 {
            // written since the snapshot, by an earlier reconcile
            owners
                .patch(
                    "owner",
                    &PatchParams::default(),
                    &Patch::Merge(serde_json::json!({ "metadata": { "annotations": {
                        ATTEMPTS_ANNOTATION: "{\"f-pool\":3}"
                    } } })),
                )
                .await
                .unwrap();
        }
        // one reconcile: both failed Jobs retried from the same snapshot
        for name in names {
            let retried = ensure_job(&ctx, &snapshot, job(name), None).await.unwrap();
            assert!(
                matches!(retried.outcome, JobOutcome::Failed(_)),
                "{name}: {:?}",
                retried.outcome
            );
        }
        let now = attempts().await;
        assert_eq!(now.get("f-c1-sync"), Some(&round), "{now:?}");
        assert_eq!(now.get("f-c2-sync"), Some(&round), "{now:?}");
        assert_eq!(now.get("f-pool"), Some(&3), "{now:?}");
        snapshot = owners.get("owner").await.unwrap();
    }
}

/// A pod of crew `f/c` with `extra` labels; nothing runs it. A
/// `finalizer` keeps it, deleting, until the test takes it off.
fn crew_pod(ns: &str, name: &str, extra: &[(&str, &str)], finalizer: Option<&str>) -> Pod {
    let mut labels = serde_json::json!({ "balerix.ai/fleet": "f", "balerix.ai/crew": "c" });
    for (k, v) in extra {
        labels[*k] = serde_json::json!(v);
    }
    serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": name, "namespace": ns, "labels": labels,
            "finalizers": finalizer.into_iter().collect::<Vec<_>>() },
        "spec": { "restartPolicy": "Never", "containers": [{ "name": "job", "image": "x" }] }
    }))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_job_is_deleted_in_the_foreground_and_holds_the_crew_until_gone() {
    use balerix_operator::controllers::jobs::ensure_job;
    use balerix_operator::desired::common::JobOutcome;
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, ctx, owner) = job_rule("stale").await;
    let client = env.client.clone();
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    // a sync Job made from older input, still running
    let old = jobs
        .create(&PostParams::default(), &crew_job(&ns, "f-c-sync", "old"))
        .await
        .unwrap();
    let ensured = ensure_job(
        &ctx,
        &owner,
        crew_job(&ns, "f-c-sync", "new"),
        Some(("f", "c")),
    )
    .await
    .unwrap();
    assert_eq!(ensured.outcome, JobOutcome::Stale);
    // deleted in the foreground: still listed, deleting, until its pods go
    let deleting = jobs.get("f-c-sync").await.unwrap();
    assert_eq!(deleting.metadata.uid, old.metadata.uid);
    assert!(deleting.metadata.deletion_timestamp.is_some());
    assert!(
        deleting
            .metadata
            .finalizers
            .unwrap_or_default()
            .contains(&"foregroundDeletion".to_string())
    );
    // the replacement waits for it, and so does a harvest of the crew
    let sync = ensure_job(
        &ctx,
        &owner,
        crew_job(&ns, "f-c-sync", "new"),
        Some(("f", "c")),
    )
    .await
    .unwrap();
    assert_eq!(sync.outcome, JobOutcome::Running);
    let harvest = ensure_job(
        &ctx,
        &owner,
        crew_job(&ns, "f-c-a-harvest", "h"),
        Some(("f", "c")),
    )
    .await
    .unwrap();
    assert_eq!(harvest.outcome, JobOutcome::Absent);
    assert!(jobs.get_opt("f-c-a-harvest").await.unwrap().is_none());
    assert_eq!(
        jobs.get("f-c-sync").await.unwrap().metadata.uid,
        old.metadata.uid
    );

    // the garbage collector: its pods gone, the Job goes; the new one is made
    support::reap_job(
        &client,
        &ns,
        "f-c-sync",
        old.metadata.uid.as_deref().unwrap(),
        Duration::from_secs(60),
    )
    .await;
    let made = ensure_job(
        &ctx,
        &owner,
        crew_job(&ns, "f-c-sync", "new"),
        Some(("f", "c")),
    )
    .await
    .unwrap();
    assert_eq!(made.outcome, JobOutcome::Running);
    let new = jobs.get("f-c-sync").await.unwrap();
    assert_ne!(new.metadata.uid, old.metadata.uid);
    assert_eq!(
        new.metadata.annotations.unwrap()["balerix.ai/input-hash"],
        "new"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crews_job_pod_without_its_job_holds_the_crew() {
    use balerix_operator::controllers::jobs::ensure_job;
    use balerix_operator::desired::common::JobOutcome;
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, ctx, owner) = job_rule("podhold").await;
    let client = env.client.clone();
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let job_name = [("batch.kubernetes.io/job-name", "f-c-sync")];
    // a dropped crew's sync pod: its Job collected in the background, the pod still running
    pods.create(
        &PostParams::default(),
        &crew_pod(&ns, "f-c-sync-x1", &job_name, None),
    )
    .await
    .unwrap();
    // an agent's own pod, and a finished pod of an old Job, hold nothing
    pods.create(&PostParams::default(), &crew_pod(&ns, "f-c-a", &[], None))
        .await
        .unwrap();
    let old = [("batch.kubernetes.io/job-name", "f-c-old")];
    pods.create(
        &PostParams::default(),
        &crew_pod(&ns, "f-c-old-x1", &old, None),
    )
    .await
    .unwrap();
    pods.patch_status(
        "f-c-old-x1",
        &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "status": { "phase": "Succeeded" } })),
    )
    .await
    .unwrap();

    let harvest = || crew_job(&ns, "f-c-a-harvest", "h");
    let held = ensure_job(&ctx, &owner, harvest(), Some(("f", "c")))
        .await
        .unwrap();
    assert_eq!(held.outcome, JobOutcome::Absent);
    assert!(jobs.get_opt("f-c-a-harvest").await.unwrap().is_none());

    // no node: the pod is deleted at once, and the harvest starts
    pods.delete("f-c-sync-x1", &DeleteParams::default())
        .await
        .unwrap();
    wait_for("the sync pod gone", Duration::from_secs(30), || async {
        pods.get_opt("f-c-sync-x1")
            .await
            .unwrap()
            .is_none()
            .then_some(())
    })
    .await;
    let made = ensure_job(&ctx, &owner, harvest(), Some(("f", "c")))
        .await
        .unwrap();
    assert_eq!(made.outcome, JobOutcome::Running);
    assert!(jobs.get_opt("f-c-a-harvest").await.unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crew_held_by_a_pod_deleting_past_the_bound_puts_crew_locked_on_the_owner() {
    use balerix_operator::controllers::jobs::ensure_job;
    use balerix_operator::desired::common::JobOutcome;
    use k8s_openapi::api::events::v1::Event;
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, ctx, owner) = job_rule("stuck").await;
    let client = env.client.clone();
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let events: Api<Event> = Api::namespaced(client.clone(), &ns);
    let job_name = [("batch.kubernetes.io/job-name", "f-c-sync")];
    // a pod on a lost node: deleting, and never gone
    pods.create(
        &PostParams::default(),
        &crew_pod(&ns, "f-c-sync-x1", &job_name, Some("balerix.ai/test-hold")),
    )
    .await
    .unwrap();
    pods.delete("f-c-sync-x1", &DeleteParams::default())
        .await
        .unwrap();

    let harvest = || crew_job(&ns, "f-c-a-harvest", "h");
    let event = wait_for(
        "a CrewLocked Event on the owner",
        Duration::from_secs(30),
        || async {
            let ensured = ensure_job(&ctx, &owner, harvest(), Some(("f", "c")))
                .await
                .unwrap();
            assert_eq!(ensured.outcome, JobOutcome::Absent, "the lock holds");
            events
                .list(&Default::default())
                .await
                .unwrap()
                .items
                .into_iter()
                .find(|e| {
                    e.reason.as_deref() == Some("CrewLocked")
                        && e.regarding.as_ref().and_then(|r| r.name.as_deref()) == Some("owner")
                })
        },
    )
    .await;
    assert_eq!(event.type_.as_deref(), Some("Warning"));
    let note = event.note.unwrap();
    assert!(
        note.starts_with("crew f/c waits on Pod f-c-sync-x1, being deleted since "),
        "{note}"
    );
    assert!(jobs.get_opt("f-c-a-harvest").await.unwrap().is_none());

    // the user forces it out: the harvest starts
    pods.patch(
        "f-c-sync-x1",
        &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "metadata": { "finalizers": null } })),
    )
    .await
    .unwrap();
    wait_for("the pod gone", Duration::from_secs(30), || async {
        pods.get_opt("f-c-sync-x1")
            .await
            .unwrap()
            .is_none()
            .then_some(())
    })
    .await;
    let made = ensure_job(&ctx, &owner, harvest(), Some(("f", "c")))
        .await
        .unwrap();
    assert_eq!(made.outcome, JobOutcome::Running);
}

/// §24.1: a stuck crew is reported once per `stuck_after`, not on every
/// 5 s wait of every waiter. The Recorder folds repeats into a series, so
/// one publish is an Event with no `series`.
#[tokio::test(flavor = "multi_thread")]
async fn crew_locked_is_published_once_per_stuck_after() {
    use balerix_operator::controllers::jobs::ensure_job;
    use k8s_openapi::api::events::v1::Event;
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, mut ctx, owner) = job_rule("throttle").await;
    // A 60 s bound, not job_rule's 1 s: the clock counts whole wall-clock
    // seconds, so five calls straddling a second boundary would otherwise
    // look like a full stuck_after elapsed and publish twice.
    ctx.run.stuck_after = Duration::from_secs(60);
    let clock = TestClock::default();
    ctx.run.clock = clock.clock();
    let client = env.client.clone();
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let events: Api<Event> = Api::namespaced(client.clone(), &ns);
    let job_name = [("batch.kubernetes.io/job-name", "f-c-sync")];
    pods.create(
        &PostParams::default(),
        &crew_pod(&ns, "f-c-sync-x1", &job_name, Some("balerix.ai/test-hold")),
    )
    .await
    .unwrap();
    pods.delete("f-c-sync-x1", &DeleteParams::default())
        .await
        .unwrap();
    clock.advance(120); // well past the 60 s stuck_after
    let locked = || async {
        events
            .list(&Default::default())
            .await
            .unwrap()
            .items
            .into_iter()
            .filter(|e| e.reason.as_deref() == Some("CrewLocked"))
            .collect::<Vec<_>>()
    };
    // five waits inside one stuck_after: one publish
    for _ in 0..5 {
        ensure_job(
            &ctx,
            &owner,
            crew_job(&ns, "f-c-a-harvest", "h"),
            Some(("f", "c")),
        )
        .await
        .unwrap();
    }
    let once = wait_for("one CrewLocked", Duration::from_secs(10), || async {
        let got = locked().await;
        (!got.is_empty()).then_some(got)
    })
    .await;
    assert_eq!(once.len(), 1);
    assert!(
        once[0].series.is_none(),
        "published more than once: {:?}",
        once[0].series
    );
    // past stuck_after again: a second publish, folded into a series
    clock.advance(120);
    ensure_job(
        &ctx,
        &owner,
        crew_job(&ns, "f-c-a-harvest", "h"),
        Some(("f", "c")),
    )
    .await
    .unwrap();
    wait_for("the repeat folded", Duration::from_secs(10), || async {
        locked().await.first()?.series.as_ref().map(|_| ())
    })
    .await;
    pods.patch(
        "f-c-sync-x1",
        &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "metadata": { "finalizers": null } })),
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_harvest_keeps_the_agent_until_it_is_purged() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, _stub, _clock, operator) = world("harvestfail").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new(
                "f",
                fleet_spec("default", &[("c", &["a", "b"])], "Branches"),
            ),
        )
        .await
        .unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    wait_for("the pod", Duration::from_secs(60), || async {
        pods.get_opt("f-c-b").await.unwrap()
    })
    .await;
    fleets
        .patch(
            "f",
            &PatchParams::default(),
            &Patch::Merge(
                serde_json::json!({ "spec": { "crews": { "c": { "agents": { "b": null } } } } }),
            ),
        )
        .await
        .unwrap();
    reap_pod(&client, &ns, "f-c-b", Duration::from_secs(60)).await;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    wait_for("the harvest Job", Duration::from_secs(60), || async {
        jobs.get_opt("f-c-b-harvest").await.unwrap()
    })
    .await;
    finish_job(
        &client,
        &ns,
        "f-c-b-harvest",
        false,
        Some("harvest: f/c/b: the clone has uncommitted work"),
    )
    .await;
    // §8.5: the failure holds the finalizer and says why, and how to skip it
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    let ready = wait_for(
        "Ready=False/HarvestFailed",
        Duration::from_secs(60),
        || async {
            let s = agents.get("f-c-b").await.unwrap().status?;
            let c = condition(&s.conditions, "Ready").clone();
            (c.reason == "HarvestFailed").then_some(c)
        },
    )
    .await;
    assert_eq!(ready.status, "False");
    assert_eq!(
        ready.message,
        "Job f-c-b-harvest failed: harvest: f/c/b: the clone has uncommitted work; the annotation balerix.ai/purge=true on the Agent skips its harvest; on the Fleet, while the Fleet is deleted, it skips every harvest and also purges the Fleet's record on the Daemon"
    );
    hold_for(
        "the Agent gone after a failed harvest",
        Duration::from_secs(3),
        || async {
            agents
                .get_opt("f-c-b")
                .await
                .unwrap()
                .is_none()
                .then_some(())
        },
    )
    .await;
    let b = agents.get("f-c-b").await.unwrap();
    assert_eq!(
        b.metadata.finalizers.as_deref(),
        Some(&["balerix.ai/harvest".to_string()][..])
    );
    Api::<PersistentVolumeClaim>::namespaced(client.clone(), &ns)
        .get("f-c-b")
        .await
        .unwrap();

    // the escape the condition names
    agents
        .patch(
            "f-c-b",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "metadata": { "annotations": { "balerix.ai/purge": "true" } } })),
        )
        .await
        .unwrap();
    wait_for("Agent f-c-b gone", Duration::from_secs(60), || async {
        agents
            .get_opt("f-c-b")
            .await
            .unwrap()
            .is_none()
            .then_some(())
    })
    .await;
    reap_claim(&client, &ns, "f-c-b", Duration::from_secs(60)).await;
    agents.get("f-c-a").await.unwrap();
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_terminating_namespace_skips_the_harvest() {
    use k8s_openapi::api::core::v1::Namespace;
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, _stub, _clock, operator) = world("terminating").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "Branches")),
        )
        .await
        .unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    wait_for("the pod", Duration::from_secs(60), || async {
        pods.get_opt("f-c-a").await.unwrap()
    })
    .await;
    Api::<PersistentVolumeClaim>::namespaced(client.clone(), &ns)
        .get("f-c-a")
        .await
        .unwrap();
    // envtest has no namespace controller: the namespace stays
    // Terminating, and the API server refuses every create in it
    Api::<Namespace>::all(client.clone())
        .delete(&ns, &Default::default())
        .await
        .unwrap();
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    agents.delete("f-c-a", &Default::default()).await.unwrap();
    reap_pod(&client, &ns, "f-c-a", Duration::from_secs(60)).await;
    wait_for("Agent f-c-a gone", Duration::from_secs(60), || async {
        agents
            .get_opt("f-c-a")
            .await
            .unwrap()
            .is_none()
            .then_some(())
    })
    .await;
    assert!(
        Api::<Job>::namespaced(client.clone(), &ns)
            .get_opt("f-c-a-harvest")
            .await
            .unwrap()
            .is_none()
    );
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_or_malformed_credentials_secret_is_on_the_agent() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, _stub, _clock, operator) = world("creds").await;
    let client = env.client.clone();
    // the Daemon names a claude credentials Secret nobody made
    Api::<Daemon>::namespaced(client.clone(), &ns)
        .patch(
            "default",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "credentials": { "claude": { "secretName": "claude-creds" } } } })),
        )
        .await
        .unwrap();
    Api::<Fleet>::namespaced(client.clone(), &ns)
        .create(
            &PostParams::default(),
            &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "Branches")),
        )
        .await
        .unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    let materialized = |reason: &'static str| {
        let agents = agents.clone();
        async move {
            let s = agents.get("f-c-a").await.unwrap().status?;
            let c = condition(&s.conditions, "Materialized").clone();
            (c.reason == reason).then_some(c)
        }
    };
    let c = wait_for("CredentialsInvalid", Duration::from_secs(60), || {
        materialized("CredentialsInvalid")
    })
    .await;
    assert_eq!(
        (c.status.as_str(), c.message.as_str()),
        (
            "False",
            "Secret claude-creds (the Daemon's spec.credentials.claude) does not exist"
        )
    );
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    assert!(pods.get_opt("f-c-a").await.unwrap().is_none());

    // a Secret whose file is not JSON: the message names the Secret and the
    // key, and never quotes what it holds
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &ns);
    let secret: Secret = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "Secret",
        "metadata": { "name": "claude-creds", "namespace": ns },
        "stringData": { "credentials.json": "sk-ant-not-json-at-all" }
    }))
    .unwrap();
    secrets
        .create(&PostParams::default(), &secret)
        .await
        .unwrap();
    let c = wait_for("the parse failure", Duration::from_secs(60), || async {
        let c = materialized("CredentialsInvalid").await?;
        c.message.contains("not JSON").then_some(c)
    })
    .await;
    assert_eq!(
        c.message,
        "Secret claude-creds key credentials.json is not JSON (line 1, column 1)"
    );
    assert!(!c.message.contains("sk-ant"));

    // fixed: the pod is made
    secrets
        .patch(
            "claude-creds",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "stringData": { "credentials.json": "{\"claudeAiOauth\":{}}" } })),
        )
        .await
        .unwrap();
    wait_for("the pod", Duration::from_secs(60), || async {
        pods.get_opt("f-c-a").await.unwrap()
    })
    .await;
    operator.abort();
}

/// The Fleet's and the Agent's `Ready` condition, once it has `reason`.
async fn ready_with<K>(
    api: &Api<K>,
    name: &str,
    reason: &str,
) -> k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition
where
    K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + serde::Serialize,
    K::DynamicType: Default,
{
    wait_for(
        &format!("{name} Ready/{reason}"),
        Duration::from_secs(60),
        || async {
            let object = serde_json::to_value(api.get(name).await.unwrap()).unwrap();
            let conditions = object["status"]["conditions"].as_array()?.clone();
            let ready = conditions.iter().find(|c| c["type"] == "Ready")?;
            (ready["reason"] == reason).then(|| serde_json::from_value(ready.clone()).unwrap())
        },
    )
    .await
}

/// §26: an object found by an expected name whose labels are not the
/// owner's is neither read, patched nor deleted, and the owner says so.
#[tokio::test(flavor = "multi_thread")]
async fn an_object_with_the_right_name_but_foreign_labels_is_left_alone() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, stub, _clock, operator) = world("foreign").await;
    let client = env.client.clone();
    let foreign = serde_json::json!({ "app": "something-else" });
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &ns);
    let secret: Secret = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "Secret",
        "metadata": { "name": "f-c-b-token", "namespace": ns, "labels": foreign },
        "stringData": { "token": "not-the-fleets" },
    }))
    .unwrap();
    let secret = secrets
        .create(&PostParams::default(), &secret)
        .await
        .unwrap();
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
    let claim: PersistentVolumeClaim = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "PersistentVolumeClaim",
        "metadata": { "name": "f-c-a", "namespace": ns, "labels": foreign },
        "spec": { "accessModes": ["ReadWriteOnce"], "resources": { "requests": { "storage": "1Gi" } } },
    }))
    .unwrap();
    let claim = claims.create(&PostParams::default(), &claim).await.unwrap();

    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new(
                "f",
                fleet_spec("default", &[("c", &["a", "b"])], "Branches"),
            ),
        )
        .await
        .unwrap();
    // the Secret is not read as the agent's token: nothing is sent
    let ready = ready_with(&fleets, "f", "Conflict").await;
    assert_eq!(ready.status, "False");
    assert!(
        ready.message.starts_with(
            "Secret f-c-b-token exists and was not made for this object (it has no label"
        ) && ready
            .message
            .ends_with("it is left alone; rename this object, or remove that one"),
        "{}",
        ready.message
    );
    hold_for("a PUT", Duration::from_secs(3), || async {
        (!stub.puts().is_empty()).then_some(())
    })
    .await;
    let kept = secrets.get("f-c-b-token").await.unwrap();
    assert_eq!(
        kept.metadata.resource_version,
        secret.metadata.resource_version
    );

    // once it is gone the Fleet mints its own
    secrets
        .delete("f-c-b-token", &DeleteParams::default())
        .await
        .unwrap();
    let put = wait_for("a PUT", Duration::from_secs(60), || async {
        stub.puts().into_iter().next()
    })
    .await;
    assert_ne!(put.agent_tokens.unwrap()["f/c/b"], "not-the-fleets");

    // the claim is not the Agent's: no pod is made on it
    crew_ready(&client, &ns, "f", "c").await;
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    let ready = ready_with(&agents, "f-c-a", "Conflict").await;
    assert!(
        ready
            .message
            .starts_with("PersistentVolumeClaim f-c-a exists and was not made for this object"),
        "{}",
        ready.message
    );
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    wait_for("the other agent's pod", Duration::from_secs(60), || async {
        pods.get_opt("f-c-b").await.unwrap()
    })
    .await;
    assert!(pods.get_opt("f-c-a").await.unwrap().is_none());

    // the agent leaves: its deletion leaves the claim as it was
    fleets
        .patch(
            "f",
            &PatchParams::default(),
            &Patch::Merge(
                serde_json::json!({ "spec": { "crews": { "c": { "agents": { "a": null } } } } }),
            ),
        )
        .await
        .unwrap();
    wait_for("Agent f-c-a gone", Duration::from_secs(60), || async {
        agents
            .get_opt("f-c-a")
            .await
            .unwrap()
            .is_none()
            .then_some(())
    })
    .await;
    let after = claims.get("f-c-a").await.unwrap();
    assert_eq!(after.metadata.uid, claim.metadata.uid);
    assert!(after.metadata.deletion_timestamp.is_none());
    assert_eq!(
        after.metadata.resource_version,
        claim.metadata.resource_version
    );
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    assert!(jobs.get_opt("f-c-a-harvest").await.unwrap().is_none());
    operator.abort();
}

/// §26.4: a claim an earlier operator made, owned by its Agent and
/// without the labels, is adopted: labelled, and released from the owner
/// reference, with the same uid.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlabelled_object_the_owner_references_is_adopted_and_labelled() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, _stub, _clock, operator) = world("adopt").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "Branches")),
        )
        .await
        .unwrap();
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    let agent = wait_for("Agent f-c-a", Duration::from_secs(60), || async {
        agents.get_opt("f-c-a").await.unwrap()
    })
    .await;
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
    let claim: PersistentVolumeClaim = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "PersistentVolumeClaim",
        "metadata": { "name": "f-c-a", "namespace": ns, "ownerReferences": [{
            "apiVersion": "balerix.ai/v1alpha1", "kind": "Agent", "name": "f-c-a",
            "uid": agent.metadata.uid.clone().unwrap(), "controller": true,
        }] },
        "spec": { "accessModes": ["ReadWriteOnce"], "resources": { "requests": { "storage": "2Gi" } } },
    }))
    .unwrap();
    let claim = claims.create(&PostParams::default(), &claim).await.unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    wait_for("the pod", Duration::from_secs(60), || async {
        pods.get_opt("f-c-a").await.unwrap()
    })
    .await;
    let adopted = wait_for("the claim released", Duration::from_secs(30), || async {
        let c = claims.get("f-c-a").await.unwrap();
        c.metadata
            .owner_references
            .as_deref()
            .unwrap_or(&[])
            .is_empty()
            .then_some(c)
    })
    .await;
    assert_eq!(adopted.metadata.uid, claim.metadata.uid);
    let labels = adopted.metadata.labels.unwrap();
    assert_eq!(labels["balerix.ai/component"], "agent");
    assert_eq!(labels["balerix.ai/fleet"], "f");
    assert_eq!(labels["balerix.ai/crew"], "c");
    assert_eq!(labels["balerix.ai/agent"], "a");
    assert_eq!(labels["balerix.ai/daemon"], "default");
    operator.abort();
}

/// #232: two agents whose names join to one object name refuse the Fleet.
#[tokio::test(flavor = "multi_thread")]
async fn colliding_names_within_a_fleet_are_refused() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, stub, _clock, operator) = world("collide").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new(
                "f",
                fleet_spec("default", &[("b", &["c-d"]), ("b-c", &["d"])], "Branches"),
            ),
        )
        .await
        .unwrap();
    let status = wait_for("Resolved=False", Duration::from_secs(60), || async {
        let s = fleets.get("f").await.unwrap().status?;
        (condition(&s.conditions, "Resolved").status == "False").then_some(s)
    })
    .await;
    let resolved = condition(&status.conditions, "Resolved");
    assert_eq!(resolved.reason, "InvalidFleet");
    assert_eq!(
        resolved.message,
        "crews.b-c.agents.d: the object name f-b-c-d is also crews.b.agents.c-d's: names are joined with `-`, so the two would be one object; rename one"
    );
    assert!(stub.puts().is_empty());
    assert!(
        Api::<Agent>::namespaced(client.clone(), &ns)
            .list(&Default::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(secrets_named(&client, &ns, "f-b-c-d-token").await.is_none());
    operator.abort();
}

async fn secrets_named(client: &kube::Client, ns: &str, name: &str) -> Option<Secret> {
    Api::<Secret>::namespaced(client.clone(), ns)
        .get_opt(name)
        .await
        .unwrap()
}

/// #232: a second Fleet whose objects take the first's names does not
/// take them over; the first keeps them, unchanged, and the second says
/// `Conflict`.
#[tokio::test(flavor = "multi_thread")]
async fn two_fleets_whose_names_meet_do_not_flip_flop() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, stub, _clock, operator) = world("meet").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new("a", fleet_spec("default", &[("b-c", &["d"])], "Branches")),
        )
        .await
        .unwrap();
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    let first = wait_for("Agent a-b-c-d", Duration::from_secs(60), || async {
        agents.get_opt("a-b-c-d").await.unwrap()
    })
    .await;
    let token = wait_for("its token", Duration::from_secs(30), || async {
        secrets_named(&client, &ns, "a-b-c-d-token").await
    })
    .await;
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new("a-b", fleet_spec("default", &[("c", &["d"])], "Branches")),
        )
        .await
        .unwrap();
    let ready = ready_with(&fleets, "a-b", "Conflict").await;
    assert_eq!(ready.status, "False");
    assert!(
        ready.message.contains("a-b-c") && ready.message.contains("was not made for this object"),
        "{}",
        ready.message
    );
    // the first Fleet's objects stay its own while both reconcile
    hold_for("the Agent taken over", Duration::from_secs(8), || async {
        let a = agents.get("a-b-c-d").await.unwrap();
        let owner = a.metadata.owner_references.clone().unwrap_or_default();
        (a.spec.fleet != "a"
            || a.spec.crew != "b-c"
            || owner.len() != 1
            || owner[0].uid != first.metadata.owner_references.as_ref().unwrap()[0].uid)
            .then_some(())
    })
    .await;
    assert_eq!(
        secrets_named(&client, &ns, "a-b-c-d-token")
            .await
            .unwrap()
            .data,
        token.data
    );
    assert!(stub.puts().iter().all(|p| p.spec.name == "a"));
    operator.abort();
}

/// What the garbage collector does under a foreground deletion, for
/// envtest, which has none: an owner's dependents deleted (the pod is the
/// kubelet's to reap) before the owner's own finalizers run.
async fn collect_dependents(client: &kube::Client, ns: &str) {
    Api::<Crew>::namespaced(client.clone(), ns)
        .delete("f-c", &DeleteParams::default())
        .await
        .unwrap();
    Api::<Agent>::namespaced(client.clone(), ns)
        .delete("f-c-a", &DeleteParams::foreground())
        .await
        .unwrap();
    reap_pod(client, ns, "f-c-a", Duration::from_secs(60)).await;
}

/// Once the harvest finalizer is off, the garbage collector's comes off
/// too (when the API server kept it: the Fleet's finalizer may have
/// deleted the Agent first, in the background).
async fn finish_foreground(client: &kube::Client, ns: &str) {
    let agents: Api<Agent> = Api::namespaced(client.clone(), ns);
    let left = wait_for(
        "the harvest finalizer off",
        Duration::from_secs(60),
        || async {
            match agents.get_opt("f-c-a").await.unwrap() {
                None => Some(None),
                Some(a) => {
                    let finalizers = a.metadata.finalizers.unwrap_or_default();
                    (!finalizers.iter().any(|f| f == "balerix.ai/harvest"))
                        .then_some(Some(finalizers))
                }
            }
        },
    )
    .await;
    if left.is_some() {
        agents
            .patch(
                "f-c-a",
                &PatchParams::default(),
                &Patch::Merge(serde_json::json!({ "metadata": { "finalizers": null } })),
            )
            .await
            .unwrap();
    }
}

/// #233: a Fleet deleted in the foreground (`kubectl delete
/// --cascade=foreground`, Argo CD's prune) with `retain: Branches`
/// still harvests: the claim has no owner for the collector to follow,
/// and the harvest Job none to be collected with.
#[tokio::test(flavor = "multi_thread")]
async fn a_foreground_deletion_with_retain_branches_still_harvests() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, stub, _clock, operator) = world("fgbranches").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "Branches")),
        )
        .await
        .unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    wait_for("the pod", Duration::from_secs(60), || async {
        pods.get_opt("f-c-a").await.unwrap()
    })
    .await;
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
    assert_eq!(
        claims.get("f-c-a").await.unwrap().metadata.owner_references,
        None
    );

    fleets
        .delete("f", &DeleteParams::foreground())
        .await
        .unwrap();
    collect_dependents(&client, &ns).await;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let harvest = wait_for("the harvest Job", Duration::from_secs(60), || async {
        jobs.get_opt("f-c-a-harvest").await.unwrap()
    })
    .await;
    assert_eq!(harvest.metadata.owner_references, None);
    finish_job(
        &client,
        &ns,
        "f-c-a-harvest",
        true,
        Some("harvested balerix/a"),
    )
    .await;
    finish_foreground(&client, &ns).await;
    // after the harvest: the claim deleted, the Job too
    wait_for("the claim deleted", Duration::from_secs(30), || async {
        match claims.get_opt("f-c-a").await.unwrap() {
            None => Some(()),
            Some(c) => c.metadata.deletion_timestamp.map(|_| ()),
        }
    })
    .await;
    // the TTL controller's to collect: envtest has none
    assert_eq!(
        harvest.spec.as_ref().unwrap().ttl_seconds_after_finished,
        Some(3600)
    );
    wait_for(
        "the Fleet's finalizer off",
        Duration::from_secs(60),
        || async {
            let f = fleets.get_opt("f").await.unwrap();
            f.is_none_or(|f| {
                !f.metadata
                    .finalizers
                    .unwrap_or_default()
                    .iter()
                    .any(|x| x == "balerix.ai/fleet")
            })
            .then_some(())
        },
    )
    .await;
    assert!(stub.deletes().contains(&"f".to_string()));
    assert!(jobs.get_opt("f-c-remove").await.unwrap().is_none());
    operator.abort();
}

/// #233: with `retain: None`, a foreground deletion still runs the crew
/// cleanup: its crews come from the Fleet's spec, not from the Crews the
/// collector deleted first, and the Job has no owner to be collected with.
#[tokio::test(flavor = "multi_thread")]
async fn a_foreground_deletion_with_retain_none_still_runs_the_crew_cleanup() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, stub, _clock, operator) = world("fgnone").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "None")),
        )
        .await
        .unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    wait_for("the pod", Duration::from_secs(60), || async {
        pods.get_opt("f-c-a").await.unwrap()
    })
    .await;

    fleets
        .delete("f", &DeleteParams::foreground())
        .await
        .unwrap();
    collect_dependents(&client, &ns).await;
    finish_foreground(&client, &ns).await;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    assert!(jobs.get_opt("f-c-a-harvest").await.unwrap().is_none());
    let remove = wait_for("the cleanup Job", Duration::from_secs(60), || async {
        jobs.get_opt("f-c-remove").await.unwrap()
    })
    .await;
    assert_eq!(remove.metadata.owner_references, None);
    assert!(stub.deletes().contains(&"f".to_string()));
    finish_job(&client, &ns, "f-c-remove", true, Some("removed")).await;
    wait_for(
        "the Fleet's finalizer off",
        Duration::from_secs(60),
        || async {
            let f = fleets.get_opt("f").await.unwrap();
            f.is_none_or(|f| {
                !f.metadata
                    .finalizers
                    .unwrap_or_default()
                    .iter()
                    .any(|x| x == "balerix.ai/fleet")
            })
            .then_some(())
        },
    )
    .await;
    // the TTL controller's to collect: envtest has none
    assert_eq!(
        remove.spec.as_ref().unwrap().ttl_seconds_after_finished,
        Some(3600)
    );
    operator.abort();
}

/// §26.3: a finished harvest Job of an earlier Agent of the same name,
/// still there until its TTL, is not this Agent's harvest: it is
/// replaced, and the claim waits for the new one.
#[tokio::test(flavor = "multi_thread")]
async fn an_earlier_agents_finished_harvest_is_not_the_next_ones() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, _stub, _clock, operator) = world("reharvest").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new(
                "f",
                fleet_spec("default", &[("c", &["a", "b"])], "Branches"),
            ),
        )
        .await
        .unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let set_agents = |agents: serde_json::Value| {
        Patch::Merge(serde_json::json!({ "spec": { "crews": { "c": { "agents": agents } } } }))
    };
    // the first Agent f-c-a: dropped, harvested, gone
    wait_for("the pod", Duration::from_secs(60), || async {
        pods.get_opt("f-c-a").await.unwrap()
    })
    .await;
    fleets
        .patch(
            "f",
            &PatchParams::default(),
            &set_agents(serde_json::json!({ "a": null })),
        )
        .await
        .unwrap();
    reap_pod(&client, &ns, "f-c-a", Duration::from_secs(60)).await;
    let first = wait_for("the first harvest", Duration::from_secs(60), || async {
        jobs.get_opt("f-c-a-harvest").await.unwrap()
    })
    .await;
    finish_job(
        &client,
        &ns,
        "f-c-a-harvest",
        true,
        Some("harvested balerix/a"),
    )
    .await;
    wait_for("the first Agent gone", Duration::from_secs(60), || async {
        agents
            .get_opt("f-c-a")
            .await
            .unwrap()
            .is_none()
            .then_some(())
    })
    .await;
    reap_claim(&client, &ns, "f-c-a", Duration::from_secs(30)).await;
    // the collector's part: the first Agent's bundle and policy go with it
    Api::<Secret>::namespaced(client.clone(), &ns)
        .delete("f-c-a-bundle", &DeleteParams::default())
        .await
        .unwrap();
    Api::<NetworkPolicy>::namespaced(client.clone(), &ns)
        .delete("f-c-a", &DeleteParams::default())
        .await
        .unwrap();

    // a second Agent of the same name, dropped within the hour
    fleets
        .patch(
            "f",
            &PatchParams::default(),
            &set_agents(serde_json::json!({ "a": {} })),
        )
        .await
        .unwrap();
    let second = wait_for("the second pod", Duration::from_secs(60), || async {
        pods.get_opt("f-c-a").await.unwrap()
    })
    .await;
    let claim = claims.get("f-c-a").await.unwrap();
    fleets
        .patch(
            "f",
            &PatchParams::default(),
            &set_agents(serde_json::json!({ "a": null })),
        )
        .await
        .unwrap();
    reap_pod_uid(
        &client,
        &ns,
        "f-c-a",
        second.metadata.uid.as_deref().unwrap(),
        Duration::from_secs(60),
    )
    .await;
    // the earlier Job is stale: deleted in the foreground, then replaced
    reap_job(
        &client,
        &ns,
        "f-c-a-harvest",
        first.metadata.uid.as_deref().unwrap(),
        Duration::from_secs(60),
    )
    .await;
    wait_for("the second harvest", Duration::from_secs(60), || async {
        let j = jobs.get_opt("f-c-a-harvest").await.unwrap()?;
        (j.metadata.uid != first.metadata.uid).then_some(())
    })
    .await;
    let kept = claims.get("f-c-a").await.unwrap();
    assert_eq!(kept.metadata.uid, claim.metadata.uid);
    assert!(
        kept.metadata.deletion_timestamp.is_none(),
        "the claim waits for its own harvest"
    );
    finish_job(
        &client,
        &ns,
        "f-c-a-harvest",
        true,
        Some("harvested balerix/a"),
    )
    .await;
    wait_for("the second Agent gone", Duration::from_secs(60), || async {
        agents
            .get_opt("f-c-a")
            .await
            .unwrap()
            .is_none()
            .then_some(())
    })
    .await;
    operator.abort();
}

/// A `retain: None` Fleet `f` with one crew, deleted; its cleanup
/// Job finished once it is the one made after `not`; returns that Job.
async fn delete_and_clean(client: &kube::Client, ns: &str, not: Option<&Job>) -> Job {
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), ns);
    fleets
        .create(
            &PostParams::default(),
            &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "None")),
        )
        .await
        .unwrap();
    let agents: Api<Agent> = Api::namespaced(client.clone(), ns);
    wait_for("Agent f-c-a", Duration::from_secs(60), || async {
        agents.get_opt("f-c-a").await.unwrap()
    })
    .await;
    // the crew's sync holds its cleanup until it is finished; envtest
    // collects nothing, so the first round's finished Jobs stay
    if not.is_none() {
        crew_ready(client, ns, "f", "c").await;
    }
    fleets.delete("f", &DeleteParams::default()).await.unwrap();
    if let Some(earlier) = not {
        reap_job(
            client,
            ns,
            "f-c-remove",
            earlier.metadata.uid.as_deref().unwrap(),
            Duration::from_secs(60),
        )
        .await;
    }
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);
    let job = wait_for("the cleanup Job", Duration::from_secs(60), || async {
        let j = jobs.get_opt("f-c-remove").await.unwrap()?;
        (j.metadata.uid != not.and_then(|n| n.metadata.uid.clone())).then_some(j)
    })
    .await;
    finish_job(client, ns, "f-c-remove", true, Some("removed")).await;
    wait_for("the Fleet gone", Duration::from_secs(60), || async {
        fleets.get_opt("f").await.unwrap().is_none().then_some(())
    })
    .await;
    // the collector's part: the Fleet's Crew goes with it
    Api::<Crew>::namespaced(client.clone(), ns)
        .delete("f-c", &DeleteParams::default())
        .await
        .unwrap();
    job
}

/// §26.3: a `retain: None` Fleet made again under the same name and
/// deleted within the hour runs its own cleanup, not the earlier one's.
#[tokio::test(flavor = "multi_thread")]
async fn a_fleet_made_again_runs_its_own_cleanup() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, _stub, _clock, operator) = world("reclean").await;
    let client = env.client.clone();
    let first = delete_and_clean(&client, &ns, None).await;
    let second = delete_and_clean(&client, &ns, Some(&first)).await;
    assert_ne!(second.metadata.uid, first.metadata.uid);
    operator.abort();
}

/// §26.1: a NetworkPolicy by the Daemon's name and with other labels
/// stops the Daemon before anything is made: no StatefulSet runs without
/// its policy.
#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_whose_policy_name_is_taken_makes_no_statefulset() {
    let Some(env) = envtest().await else { return };
    let ns = namespace(&env.client, "policytaken").await;
    let client = env.client.clone();
    let policy: NetworkPolicy = serde_json::from_value(serde_json::json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
        "metadata": { "name": "balerix-default", "namespace": ns,
            "labels": { "app": "something-else" } },
        "spec": { "podSelector": {}, "policyTypes": ["Ingress"] },
    }))
    .unwrap();
    let policies: Api<NetworkPolicy> = Api::namespaced(client.clone(), &ns);
    let policy = policies
        .create(&PostParams::default(), &policy)
        .await
        .unwrap();
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
    let ready = ready_with(&daemons, "default", "Conflict").await;
    assert!(
        ready
            .message
            .starts_with("NetworkPolicy balerix-default exists and was not made for this object"),
        "{}",
        ready.message
    );
    let statefulsets: Api<StatefulSet> = Api::namespaced(client.clone(), &ns);
    hold_for("a StatefulSet", Duration::from_secs(3), || async {
        statefulsets.get_opt("balerix-default").await.unwrap()
    })
    .await;
    assert_eq!(
        policies
            .get("balerix-default")
            .await
            .unwrap()
            .metadata
            .resource_version,
        policy.metadata.resource_version
    );
    operator.abort();
}

/// §26.1: a write goes only over the object read. One deleted and made
/// again under the name since is refused (its uid differs), and so is a
/// create over an object made since a read found none.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_over_an_object_replaced_since_it_was_read_is_refused() {
    use balerix_operator::controllers::owned::apply_read;
    let Some(env) = envtest().await else { return };
    let ns = namespace(&env.client, "readwrite").await;
    let maps: Api<ConfigMap> = Api::namespaced(env.client.clone(), &ns);
    let map = |value: &str| -> ConfigMap {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": { "name": "m", "namespace": ns },
            "data": { "k": value },
        }))
        .unwrap()
    };
    let read = apply_read(&maps, &map("one"), None).await.unwrap();
    // over the object read: written
    apply_read(&maps, &map("two"), Some(&read)).await.unwrap();
    assert_eq!(maps.get("m").await.unwrap().data.unwrap()["k"], "two");
    // a read that found none, and an object made since: a 409
    let e = apply_read(&maps, &map("three"), None).await.unwrap_err();
    assert!(e.to_string().contains("already exists"), "{e}");
    // made again under the name since the read: refused, left as it is
    maps.delete("m", &DeleteParams::default()).await.unwrap();
    maps.create(&PostParams::default(), &map("theirs"))
        .await
        .unwrap();
    let e = apply_read(&maps, &map("four"), Some(&read))
        .await
        .unwrap_err();
    assert!(e.to_string().to_lowercase().contains("uid"), "{e}");
    assert_eq!(maps.get("m").await.unwrap().data.unwrap()["k"], "theirs");
}

/// §4.2: a Fleet named `balerix` or with the `balerix-` prefix is
/// refused at admission (the CRD's CEL rules), so nothing is made for it;
/// resolution refuses it too, for a cluster whose definitions are older.
#[tokio::test(flavor = "multi_thread")]
async fn a_fleet_with_the_reserved_prefix_is_refused() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, stub, _clock, operator) = world("reserved").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    for (name, why) in [
        (
            "balerix-x",
            "the balerix- prefix is reserved for the operator's own objects",
        ),
        ("balerix", "the name balerix is reserved"),
    ] {
        let e = fleets
            .create(
                &PostParams::default(),
                &Fleet::new(name, fleet_spec("default", &[("c", &["a"])], "Branches")),
            )
            .await
            .unwrap_err();
        assert!(e.to_string().contains(why), "{name}: {e}");
    }
    hold_for("an object for it", Duration::from_secs(3), || async {
        let agents = Api::<Agent>::namespaced(client.clone(), &ns)
            .list(&Default::default())
            .await
            .unwrap();
        let crews = Api::<Crew>::namespaced(client.clone(), &ns)
            .list(&Default::default())
            .await
            .unwrap();
        (!agents.items.is_empty() || !crews.items.is_empty() || !stub.puts().is_empty())
            .then_some(())
    })
    .await;
    assert!(
        secrets_named(&client, &ns, "balerix-x-c-a-token")
            .await
            .is_none()
    );
    operator.abort();
}

/// The Fleet definition with its `x-kubernetes-validations` taken out
/// (`strip`) or put back from the Rust types.
async fn fleet_rules(client: &kube::Client, strip: bool) {
    use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
    use kube::CustomResourceExt;
    let rules = |c: &CustomResourceDefinition| {
        c.spec.versions[0]
            .schema
            .as_ref()
            .and_then(|s| s.open_api_v3_schema.as_ref())
            .and_then(|s| s.x_kubernetes_validations.clone())
    };
    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
    let mut crd = crds.get("fleets.balerix.ai").await.unwrap();
    let wanted = if strip { None } else { rules(&Fleet::crd()) };
    if let Some(schema) = crd.spec.versions[0]
        .schema
        .as_mut()
        .and_then(|s| s.open_api_v3_schema.as_mut())
    {
        schema.x_kubernetes_validations = wanted;
    }
    crds.replace("fleets.balerix.ai", &PostParams::default(), &crd)
        .await
        .unwrap();
}

/// §4.2: the reserved-name rules hold on create only. A Fleet
/// named before them can still have its status and metadata written and
/// be deleted; resolution marks it `InvalidFleet`.
#[tokio::test(flavor = "multi_thread")]
async fn a_fleet_named_before_the_rules_can_still_be_written_and_deleted() {
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, _stub, _clock, operator) = world("oldname").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleet_rules(&client, true).await;
    // the definition's change takes a moment to reach the validator
    wait_for("the rules gone", Duration::from_secs(30), || async {
        fleets
            .create(
                &PostParams::default(),
                &Fleet::new(
                    "balerix-old",
                    fleet_spec("default", &[("c", &["a"])], "Branches"),
                ),
            )
            .await
            .ok()
    })
    .await;
    fleet_rules(&client, false).await;
    wait_for("the rules back", Duration::from_secs(30), || async {
        fleets
            .create(
                &PostParams::default(),
                &Fleet::new(
                    "balerix-new",
                    fleet_spec("default", &[("c", &["a"])], "Branches"),
                ),
            )
            .await
            .err()
    })
    .await;
    let status = wait_for("Resolved=False", Duration::from_secs(60), || async {
        let s = fleets.get("balerix-old").await.unwrap().status?;
        (condition(&s.conditions, "Resolved").reason == "InvalidFleet").then_some(s)
    })
    .await;
    assert_eq!(
        condition(&status.conditions, "Resolved").message,
        "name: \"balerix-old\": the `balerix-` prefix is reserved for the operator's own objects"
    );
    fleets
        .patch(
            "balerix-old",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "metadata": { "annotations": { "x": "y" } } })),
        )
        .await
        .unwrap();
    fleets
        .delete("balerix-old", &DeleteParams::default())
        .await
        .unwrap();
    wait_for("the Fleet gone", Duration::from_secs(60), || async {
        fleets
            .get_opt("balerix-old")
            .await
            .unwrap()
            .is_none()
            .then_some(())
    })
    .await;
    operator.abort();
}

#[test]
fn the_harness_sweeps_the_roots_of_dead_test_processes() {
    let tmp = support::temp_root("sweep");
    let mut child = std::process::Command::new("true").spawn().unwrap();
    child.wait().unwrap();
    let dead = tmp.join(format!("envtest-{}", child.id()));
    let live = tmp.join(format!("envtest-{}", std::process::id()));
    let other = tmp.join("client-real-daemon-1");
    for dir in [&dead, &live, &other] {
        std::fs::create_dir_all(dir.join("etcd")).unwrap();
    }
    let removed = support::envtest::sweep_dead_roots(&tmp);
    assert_eq!(removed, vec![dead.clone()]);
    assert!(!dead.exists() && live.is_dir() && other.is_dir());
    std::fs::remove_dir_all(&tmp).unwrap();
}

/// A ConfigMap `m` with these labels, data and controller owner uid.
fn owned_map(ns: &str, labels: &[&str], data: &[&str], owner: &str) -> ConfigMap {
    let pairs = |keys: &[&str]| -> serde_json::Map<String, serde_json::Value> {
        keys.iter()
            .map(|k| (k.to_string(), serde_json::json!("v")))
            .collect()
    };
    serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": { "name": "m", "namespace": ns, "labels": pairs(labels),
            "ownerReferences": [{ "apiVersion": "v1", "kind": "ConfigMap", "name": "o",
                "uid": owner, "controller": true }] },
        "data": pairs(data),
    }))
    .unwrap()
}

/// §26.1: an object the operator created is the operator's to change: a
/// label or key the desired object drops goes on the next write, and so
/// does a controller reference to an earlier owner (a leftover, or one
/// made by a create no apply followed).
#[tokio::test(flavor = "multi_thread")]
async fn a_field_dropped_from_the_desired_object_goes_on_the_next_write() {
    use balerix_operator::controllers::owned::apply_read;
    let Some(env) = envtest().await else { return };
    let ns = namespace(&env.client, "dropfield").await;
    let maps: Api<ConfigMap> = Api::namespaced(env.client.clone(), &ns);
    let read = apply_read(
        &maps,
        &owned_map(&ns, &["keep", "drop"], &["a", "b"], "u1"),
        None,
    )
    .await
    .unwrap();
    let written = apply_read(&maps, &owned_map(&ns, &["keep"], &["a"], "u1"), Some(&read))
        .await
        .unwrap();
    assert_eq!(
        written
            .metadata
            .labels
            .as_ref()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["keep"]
    );
    assert_eq!(
        written.data.as_ref().unwrap().keys().collect::<Vec<_>>(),
        ["a"]
    );

    // a merge patch's field (an attempts annotation) is the patch's: a
    // later write keeps it
    let patched = maps
        .patch(
            "m",
            &balerix_operator::controllers::owned::merge_params(),
            &Patch::Merge(serde_json::json!({ "metadata": { "annotations": { "balerix.ai/attempts": "{}" } } })),
        )
        .await
        .unwrap();
    let kept = apply_read(
        &maps,
        &owned_map(&ns, &["keep"], &["a"], "u1"),
        Some(&patched),
    )
    .await
    .unwrap();
    assert_eq!(
        kept.metadata.annotations.as_ref().unwrap()["balerix.ai/attempts"],
        "{}"
    );
    let written = kept;

    // a later owner of the same names: the earlier one's reference goes
    let again = apply_read(
        &maps,
        &owned_map(&ns, &["keep"], &["a"], "u2"),
        Some(&written),
    )
    .await
    .unwrap();
    let owners = again.metadata.owner_references.unwrap();
    assert_eq!((owners.len(), owners[0].uid.as_str()), (1, "u2"));

    // made by the operator's own create, which no apply followed (a pass
    // that ended between the two): the same
    maps.delete("m", &DeleteParams::default()).await.unwrap();
    let operator_create = PostParams {
        field_manager: Some("balerix-operator".into()),
        ..PostParams::default()
    };
    let made = maps
        .create(
            &operator_create,
            &owned_map(&ns, &["keep", "drop"], &["a", "b"], "u3"),
        )
        .await
        .unwrap();
    let written = apply_read(&maps, &owned_map(&ns, &["keep"], &["a"], "u4"), Some(&made))
        .await
        .unwrap();
    let owners = written.metadata.owner_references.clone().unwrap();
    assert_eq!((owners.len(), owners[0].uid.as_str()), (1, "u4"));
    assert_eq!(
        written.metadata.labels.unwrap().keys().collect::<Vec<_>>(),
        ["keep"]
    );
    assert_eq!(written.data.unwrap().keys().collect::<Vec<_>>(), ["a"]);

    // a controller reference someone else set is theirs: a Conflict, and
    // the object is left as it was
    maps.delete("m", &DeleteParams::default()).await.unwrap();
    let theirs = maps
        .create(
            &PostParams::default(),
            &owned_map(&ns, &["keep"], &["a"], "u5"),
        )
        .await
        .unwrap();
    let e = apply_read(
        &maps,
        &owned_map(&ns, &["keep"], &["a"], "u6"),
        Some(&theirs),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&e, balerix_operator::controllers::Error::Conflict(m)
            if m.contains("controller owner reference to another owner")),
        "{e}"
    );
    let left = maps
        .get("m")
        .await
        .unwrap()
        .metadata
        .owner_references
        .unwrap();
    assert_eq!((left.len(), left[0].uid.as_str()), (1, "u5"));
}
