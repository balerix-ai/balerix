#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Plugin controller and the Daemon controller's plugin half against a
//! real API server (Spec O §23.4, §23.6), the Daemon a stub.
mod support;

use std::time::Duration;

use balerix_api::{AgentPhase, PluginStatus};
use balerix_operator::api::{Daemon, DaemonSpec, Plugin, PluginSpec};
use balerix_operator::pki;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::Api;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use support::envtest::envtest;
use support::stub_daemon::StubDaemon;
use support::{TestClock, make_daemon_ready, namespace, spawn_operator, wait_for};

fn daemon_spec(plugins: &[&str]) -> DaemonSpec {
    serde_json::from_value(serde_json::json!({
        "storage": { "state": { "size": "1Gi" }, "shared": { "size": "10Gi" }, "agent": { "size": "2Gi" } },
        "plugins": plugins,
    }))
    .unwrap()
}

fn plugin_spec(extra: serde_json::Value) -> PluginSpec {
    let mut spec = serde_json::json!({ "image": "balerix-plugin-web:test", "needs": ["fleets", "actions"],
        "config": { "enabled": true } });
    for (k, v) in extra.as_object().unwrap() {
        spec[k] = v.clone();
    }
    serde_json::from_value(spec).unwrap()
}

fn ready_row(name: &str) -> PluginStatus {
    PluginStatus {
        name: name.into(),
        version: "1".into(),
        phase: AgentPhase::Ready,
        listen: None,
        routes: true,
        message: String::new(),
        active_agents: 0,
    }
}

fn condition(p: &Plugin, type_: &str) -> Option<(String, String, String)> {
    let c = p
        .status
        .as_ref()?
        .conditions
        .iter()
        .find(|c| c.type_ == type_)?;
    Some((c.status.clone(), c.reason.clone(), c.message.clone()))
}

/// A Daemon listing `plugins`, its kubelet steps done, a stub, an operator.
async fn world(
    label: &str,
    plugins: &[&str],
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
            &Daemon::new("default", daemon_spec(plugins)),
        )
        .await
        .unwrap();
    let stub = StubDaemon::start().await;
    let clock = TestClock::default();
    let operator = spawn_operator(env, &ns, Some(stub.url()), &clock);
    make_daemon_ready(&env.client, &ns).await;
    (env, ns, stub, clock, operator)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unlisted_plugin_owns_nothing() {
    let (env, ns, _stub, _clock, operator) = world("unlisted", &[]).await;
    let plugins: Api<Plugin> = Api::namespaced(env.client.clone(), &ns);
    plugins
        .create(
            &PostParams::default(),
            &Plugin::new("web", plugin_spec(serde_json::json!({}))),
        )
        .await
        .unwrap();
    let got = wait_for(
        "Deployed=False/NotListed",
        Duration::from_secs(30),
        || async {
            let p = plugins.get("web").await.unwrap();
            condition(&p, "Deployed").filter(|c| c.1 == "NotListed")
        },
    )
    .await;
    assert_eq!(
        got.2,
        "no Daemon in this namespace lists it in spec.plugins"
    );
    let deployments: Api<Deployment> = Api::namespaced(env.client.clone(), &ns);
    assert!(
        deployments
            .get_opt("balerix-plugin-web")
            .await
            .unwrap()
            .is_none()
    );
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listed_plugin_gets_its_objects_and_a_changed_input_rolls_it() {
    let (env, ns, stub, _clock, operator) = world("listed", &["web"]).await;
    let c = env.client.clone();
    let plugins: Api<Plugin> = Api::namespaced(c.clone(), &ns);
    plugins
        .create(
            &PostParams::default(),
            &Plugin::new(
                "web",
                plugin_spec(
                    serde_json::json!({ "expose": { "port": 8080 }, "scratch": { "size": "1Gi" } }),
                ),
            ),
        )
        .await
        .unwrap();
    let deployments: Api<Deployment> = Api::namespaced(c.clone(), &ns);
    let first = wait_for("the Deployment", Duration::from_secs(60), || async {
        deployments.get_opt("balerix-plugin-web").await.unwrap()
    })
    .await;
    for (kind, found) in [
        (
            "token",
            Api::<Secret>::namespaced(c.clone(), &ns)
                .get_opt("balerix-plugin-web-token")
                .await
                .unwrap()
                .is_some(),
        ),
        (
            "serving",
            Api::<Secret>::namespaced(c.clone(), &ns)
                .get_opt("balerix-plugin-web-tls")
                .await
                .unwrap()
                .is_some(),
        ),
        (
            "service",
            Api::<Service>::namespaced(c.clone(), &ns)
                .get_opt("web")
                .await
                .unwrap()
                .is_some(),
        ),
        (
            "policy",
            Api::<NetworkPolicy>::namespaced(c.clone(), &ns)
                .get_opt("balerix-plugin-web")
                .await
                .unwrap()
                .is_some(),
        ),
        (
            "claim",
            Api::<PersistentVolumeClaim>::namespaced(c.clone(), &ns)
                .get_opt("balerix-plugin-web-scratch")
                .await
                .unwrap()
                .is_some(),
        ),
    ] {
        assert!(found, "{kind}");
    }
    // the serving certificate verifies under the Daemon's authority for the Service's name
    let ca = Api::<ConfigMap>::namespaced(c.clone(), &ns)
        .get("balerix-default-ca")
        .await
        .unwrap()
        .data
        .unwrap()["ca.crt"]
        .clone();
    let serving = Api::<Secret>::namespaced(c.clone(), &ns)
        .get("balerix-plugin-web-tls")
        .await
        .unwrap();
    let cert = String::from_utf8(serving.data.unwrap()["tls.crt"].0.clone()).unwrap();
    assert!(pki::verifies(
        &ca,
        &cert,
        &format!("web.{ns}.svc"),
        support::now()
    ));

    // the kubelet: one available replica; the Daemon: a ready row
    deployments
        .patch_status(
            "balerix-plugin-web",
            &PatchParams::default(),
            &Patch::Merge(
                serde_json::json!({ "status": { "replicas": 1, "readyReplicas": 1, "availableReplicas": 1 } }),
            ),
        )
        .await
        .unwrap();
    stub.set_plugin_rows(vec![ready_row("web")]);
    wait_for("Deployed and Ready", Duration::from_secs(30), || async {
        let p = plugins.get("web").await.unwrap();
        (condition(&p, "Deployed")?.0 == "True" && condition(&p, "Ready")?.0 == "True")
            .then_some(())
    })
    .await;

    // a config change is a new revision: the pod template's hash moves
    let hash = |d: &Deployment| {
        d.spec
            .as_ref()
            .unwrap()
            .template
            .metadata
            .as_ref()
            .unwrap()
            .annotations
            .as_ref()
            .unwrap()["balerix.ai/input-hash"]
            .clone()
    };
    plugins
        .patch(
            "web",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "config": { "enabled": false } } })),
        )
        .await
        .unwrap();
    wait_for(
        "a new pod-template hash",
        Duration::from_secs(30),
        || async {
            let d = deployments.get("balerix-plugin-web").await.unwrap();
            (hash(&d) != hash(&first)).then_some(())
        },
    )
    .await;
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_authority_reissues_the_plugins_certificate() {
    let (env, ns, _stub, _clock, operator) = world("authority", &["web"]).await;
    let c = env.client.clone();
    Api::<Plugin>::namespaced(c.clone(), &ns)
        .create(
            &PostParams::default(),
            &Plugin::new("web", plugin_spec(serde_json::json!({}))),
        )
        .await
        .unwrap();
    let secrets: Api<Secret> = Api::namespaced(c.clone(), &ns);
    let cert = |s: Secret| String::from_utf8(s.data.unwrap()["tls.crt"].0.clone()).unwrap();
    let before = wait_for("the serving Secret", Duration::from_secs(60), || async {
        secrets.get_opt("balerix-plugin-web-tls").await.unwrap()
    })
    .await;
    // a new authority: delete it, and the Daemon controller mints another (§22.4)
    secrets
        .delete("balerix-default-ca", &DeleteParams::default())
        .await
        .unwrap();
    let after = wait_for(
        "a reissued certificate",
        Duration::from_secs(60),
        || async {
            let now = secrets.get("balerix-plugin-web-tls").await.ok()?;
            (cert(now.clone()) != cert(before.clone())).then_some(now)
        },
    )
    .await;
    let ca = Api::<ConfigMap>::namespaced(c.clone(), &ns)
        .get("balerix-default-ca")
        .await
        .unwrap()
        .data
        .unwrap()["ca.crt"]
        .clone();
    assert!(pki::verifies(
        &ca,
        &cert(after),
        &format!("web.{ns}.svc"),
        support::now()
    ));
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_secret_blocks_the_plugin_and_names_the_key() {
    let (env, ns, _stub, _clock, operator) = world("secret", &["web"]).await;
    let plugins: Api<Plugin> = Api::namespaced(env.client.clone(), &ns);
    plugins
        .create(
            &PostParams::default(),
            &Plugin::new(
                "web",
                plugin_spec(serde_json::json!({
        "secrets": { "password": { "secretName": "creds", "key": "pw" } } })),
            ),
        )
        .await
        .unwrap();
    let got = wait_for("SecretMissing", Duration::from_secs(30), || async {
        condition(&plugins.get("web").await.unwrap(), "Deployed").filter(|c| c.1 == "SecretMissing")
    })
    .await;
    assert_eq!(got.2, "spec.secrets.password: Secret creds does not exist");
    // the key absent from an existing Secret is named too
    Api::<Secret>::namespaced(env.client.clone(), &ns).create(&PostParams::default(), &serde_json::from_value(
        serde_json::json!({ "metadata": { "name": "creds" }, "stringData": { "other": "x" } })).unwrap()).await.unwrap();
    wait_for("the key named", Duration::from_secs(30), || async {
        condition(&plugins.get("web").await.unwrap(), "Deployed")
            .filter(|c| c.2 == "spec.secrets.password: Secret creds has no key pw")
    })
    .await;
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_unlisted_owns_nothing_and_relisted_comes_back() {
    let (env, ns, _stub, _clock, operator) = world("relist", &["web"]).await;
    let c = env.client.clone();
    Api::<Plugin>::namespaced(c.clone(), &ns)
        .create(
            &PostParams::default(),
            &Plugin::new("web", plugin_spec(serde_json::json!({}))),
        )
        .await
        .unwrap();
    let secrets: Api<Secret> = Api::namespaced(c.clone(), &ns);
    let token = |s: Secret| s.data.unwrap()["token"].0.clone();
    let first = wait_for("the token", Duration::from_secs(60), || async {
        secrets.get_opt("balerix-plugin-web-token").await.unwrap()
    })
    .await;
    let daemons: Api<Daemon> = Api::namespaced(c.clone(), &ns);
    daemons
        .patch(
            "default",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "plugins": [] } })),
        )
        .await
        .unwrap();
    let deployments: Api<Deployment> = Api::namespaced(c.clone(), &ns);
    wait_for("objects gone", Duration::from_secs(30), || async {
        (deployments
            .get_opt("balerix-plugin-web")
            .await
            .unwrap()
            .is_none()
            && secrets
                .get_opt("balerix-plugin-web-token")
                .await
                .unwrap()
                .is_none())
        .then_some(())
    })
    .await;
    daemons
        .patch(
            "default",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "plugins": ["web"] } })),
        )
        .await
        .unwrap();
    let again = wait_for("a new token", Duration::from_secs(60), || async {
        secrets.get_opt("balerix-plugin-web-token").await.unwrap()
    })
    .await;
    assert_ne!(token(again), token(first));
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_listed_by_two_daemons_is_neither_ones() {
    let (env, ns, _stub, _clock, operator) = world("twice", &["web"]).await;
    let c = env.client.clone();
    Api::<Daemon>::namespaced(c.clone(), &ns)
        .create(
            &PostParams::default(),
            &Daemon::new("other", daemon_spec(&["web"])),
        )
        .await
        .unwrap();
    let plugins: Api<Plugin> = Api::namespaced(c.clone(), &ns);
    plugins
        .create(
            &PostParams::default(),
            &Plugin::new("web", plugin_spec(serde_json::json!({}))),
        )
        .await
        .unwrap();
    let got = wait_for("PluginListedTwice", Duration::from_secs(30), || async {
        condition(&plugins.get("web").await.unwrap(), "Deployed")
            .filter(|c| c.1 == "PluginListedTwice")
    })
    .await;
    assert_eq!(got.2, "listed by Daemons default, other");
    operator.abort();
}
