#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The controllers against a real API server (Spec O §21.3). The test is
//! the kubelet: it patches Job and Pod status and force-deletes pods.
mod support;

use std::time::Duration;

use balerix_operator::api::{Daemon, DaemonSpec};
use kube::Api;
use kube::api::PostParams;
use support::envtest::envtest;
use support::{TestClock, namespace, spawn_operator, wait_for};

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
