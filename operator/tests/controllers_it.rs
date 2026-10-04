#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The controllers against a real API server (Spec O §21.3). The test is
//! the kubelet: it patches Job and Pod status and force-deletes pods.
mod support;

use std::time::Duration;

use balerix_api::{AgentPhase, AgentStatus as DaemonAgentStatus};
use balerix_operator::api::{Agent, Crew, Daemon, DaemonSpec, Fleet, FleetSpec};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Pod, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::Api;
use kube::api::{Patch, PatchParams, PostParams};
use support::envtest::envtest;
use support::stub_daemon::StubDaemon;
use support::{
    TestClock, finish_job, hold_for, namespace, reap_claim, reap_pod, reap_pod_uid, spawn_operator,
    wait_for,
};

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
    wait_for("a new authority", Duration::from_secs(60), || async {
        let s = secrets.get_opt("balerix-default-ca").await.unwrap()?;
        (s.data.as_ref()?.get("ca.crt") != ca.data.as_ref().unwrap().get("ca.crt")).then_some(())
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
    let reissued_not_after =
        reissued.metadata.annotations.as_ref().unwrap()["balerix.ai/not-after"].clone();
    wait_for(
        "the pod template to follow",
        Duration::from_secs(60),
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
    let second = wait_for("the retry", Duration::from_secs(60), || async {
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
    wait_for("the StatefulSet", Duration::from_secs(60), || async {
        statefulsets.get_opt("balerix-default").await.unwrap()
    })
    .await;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    wait_for("the pool Job", Duration::from_secs(60), || async {
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
    let second = wait_for("the retry", Duration::from_secs(60), || async {
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
    let _ = second;
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
            "Agent {ns}/f-d-a: harvest Job f-d-a-harvest not finished; the annotation balerix.ai/purge=true on the Agent or its Fleet skips the harvest"
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
    let a = wait_for("Agent f-d-a", Duration::from_secs(60), || async {
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
    let _ = a;
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
    let status = wait_for("phase ready", Duration::from_secs(60), || async {
        let s = agents.get("f-c-a").await.unwrap().status?;
        (s.phase.as_deref() == Some("ready")).then_some(s)
    })
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
        "Job f-c-b-harvest failed: harvest: f/c/b: the clone has uncommitted work; the annotation balerix.ai/purge=true on the Agent or its Fleet skips the harvest"
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
