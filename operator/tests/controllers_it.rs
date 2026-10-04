#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The controllers against a real API server (Spec O §21.3). The test is
//! the kubelet: it patches Job and Pod status and force-deletes pods.
mod support;

use std::time::Duration;

use balerix_operator::api::{Agent, Crew, Daemon, DaemonSpec, Fleet, FleetSpec};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::Api;
use kube::api::{Patch, PatchParams, PostParams};
use support::envtest::envtest;
use support::stub_daemon::StubDaemon;
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
    let renewed = wait_for("a renewed certificate", Duration::from_secs(30), || async {
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

    // a lost authority is minted again, and the serving certificate with it
    secrets
        .delete("balerix-default-ca", &Default::default())
        .await
        .unwrap();
    wait_for("a new authority", Duration::from_secs(30), || async {
        let s = secrets.get_opt("balerix-default-ca").await.unwrap()?;
        (s.data.as_ref()?.get("ca.crt") != ca.data.as_ref().unwrap().get("ca.crt")).then_some(())
    })
    .await;
    let reissued = wait_for(
        "a serving certificate from the new authority",
        Duration::from_secs(30),
        || async {
            let s = secrets.get("balerix-default-tls").await.unwrap();
            (s.data.as_ref().unwrap()["tls.crt"] != renewed.1.data.as_ref().unwrap()["tls.crt"])
                .then_some(s)
        },
    )
    .await;
    let reissued_not_after =
        reissued.metadata.annotations.as_ref().unwrap()["balerix.ai/not-after"].clone();
    wait_for(
        "the pod template to follow",
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
                == reissued_not_after)
                .then_some(())
        },
    )
    .await;
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
    let statefulsets: Api<StatefulSet> = Api::namespaced(client.clone(), &ns);
    wait_for("the StatefulSet", Duration::from_secs(30), || async {
        statefulsets.get_opt("balerix-default").await.unwrap()
    })
    .await;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    wait_for("the pool Job", Duration::from_secs(30), || async {
        jobs.get_opt("balerix-default-pool").await.unwrap()
    })
    .await;

    // the kubelet: the pool synced, the claim bound, the pod ready
    finish_job(&client, &ns, "balerix-default-pool", true, Some("synced")).await;
    Api::<PersistentVolumeClaim>::namespaced(client.clone(), &ns)
        .patch_status(
            "balerix-default-shared",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "status": { "phase": "Bound" } })),
        )
        .await
        .unwrap();
    statefulsets
        .patch_status(
            "balerix-default",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "status": { "replicas": 1, "readyReplicas": 1 } })),
        )
        .await
        .unwrap();

    let ready = wait_for(
        "Ready=False/DaemonNotReady",
        Duration::from_secs(30),
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
    wait_for("the Crew", Duration::from_secs(30), || async {
        crews.get_opt("f-c").await.unwrap()
    })
    .await;
    let a = wait_for("Agent f-c-a", Duration::from_secs(10), || async {
        agents.get_opt("f-c-a").await.unwrap()
    })
    .await;
    agents.get("f-c-b").await.unwrap();
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
            .filter(|s| s.conditions.len() == 3)
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
    let status = wait_for("Accepted=False", Duration::from_secs(30), || async {
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
    wait_for("the Crew", Duration::from_secs(30), || async {
        crews.get_opt("f-c").await.unwrap()
    })
    .await;
    stub.stop().await;
    let status = wait_for("DaemonUnavailable", Duration::from_secs(30), || async {
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
    let status = wait_for("Resolved=False", Duration::from_secs(30), || async {
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
