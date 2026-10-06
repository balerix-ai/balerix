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
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::{Api, ResourceExt};
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
    // a Service of the plugin's name that someone else made
    let services: Api<Service> = Api::namespaced(env.client.clone(), &ns);
    let theirs: Service = serde_json::from_value(serde_json::json!({
        "metadata": { "name": "web" }, "spec": { "ports": [{ "port": 80 }] } }))
    .unwrap();
    services
        .create(&PostParams::default(), &theirs)
        .await
        .unwrap();
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
    // the reconcile that said NotListed released what it owns first: not this
    assert!(
        services.get_opt("web").await.unwrap().is_some(),
        "their Service survives"
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
    // a new authority: delete it, and the Daemon controller mints another
    // (§22.4). A reconcile that read the CA before the delete applies it
    // again, so the delete is repeated while the old one is back
    let configmaps: Api<ConfigMap> = Api::namespaced(c.clone(), &ns);
    let ca_of = |m: ConfigMap| m.data.unwrap()["ca.crt"].clone();
    let original = ca_of(configmaps.get("balerix-default-ca").await.unwrap());
    wait_for("a new authority", Duration::from_secs(60), || async {
        if ca_of(configmaps.get("balerix-default-ca").await.ok()?) != original {
            return Some(());
        }
        let old = secrets
            .get_opt("balerix-default-ca")
            .await
            .ok()?
            .is_some_and(|s| {
                s.data
                    .is_some_and(|d| d.get("ca.crt").is_some_and(|v| v.0 == original.as_bytes()))
            });
        if old {
            match secrets
                .delete("balerix-default-ca", &DeleteParams::default())
                .await
            {
                Ok(_) => {}
                Err(kube::Error::Api(e)) if e.code == 404 => {}
                Err(e) => panic!("deleting the CA Secret: {e}"),
            }
        }
        None
    })
    .await;
    let after = wait_for(
        "a reissued certificate",
        Duration::from_secs(60),
        || async {
            let now = secrets.get("balerix-plugin-web-tls").await.ok()?;
            (cert(now.clone()) != cert(before.clone())).then_some(now)
        },
    )
    .await;
    let ca = ca_of(configmaps.get("balerix-default-ca").await.unwrap());
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
    let (env, ns, stub, _clock, operator) = world("relist", &["web"]).await;
    let c = env.client.clone();
    let plugins: Api<Plugin> = Api::namespaced(c.clone(), &ns);
    plugins
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
    let services: Api<Service> = Api::namespaced(c.clone(), &ns);
    let policies: Api<NetworkPolicy> = Api::namespaced(c.clone(), &ns);
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
                .is_none()
            && secrets
                .get_opt("balerix-plugin-web-tls")
                .await
                .unwrap()
                .is_none()
            && services.get_opt("web").await.unwrap().is_none()
            && policies
                .get_opt("balerix-plugin-web")
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

    // relisted, it runs again: no operator restart
    wait_for("the Deployment again", Duration::from_secs(30), || async {
        deployments.get_opt("balerix-plugin-web").await.unwrap()
    })
    .await;
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
    wait_for("Ready again", Duration::from_secs(30), || async {
        let p = plugins.get("web").await.unwrap();
        (condition(&p, "Ready")?.0 == "True").then_some(())
    })
    .await;
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

fn made(stub: &StubDaemon) -> Vec<Vec<String>> {
    stub.lists()
        .iter()
        .map(|l| l.plugins.iter().map(|p| p.name.clone()).collect())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_list_is_sent_on_every_reconcile_in_spec_order() {
    let (env, ns, stub, _clock, operator) = world("list", &["web", "flow"]).await;
    let plugins: Api<Plugin> = Api::namespaced(env.client.clone(), &ns);
    for name in ["flow", "web"] {
        plugins
            .create(
                &PostParams::default(),
                &Plugin::new(name, plugin_spec(serde_json::json!({}))),
            )
            .await
            .unwrap();
    }
    wait_for(
        "three lists naming web then flow",
        Duration::from_secs(60),
        || async {
            (made(&stub)
                .iter()
                .filter(|l| *l == &["web", "flow"])
                .count()
                >= 3)
                .then_some(())
        },
    )
    .await;
    let list = stub.lists().last().unwrap().clone();
    assert_eq!(list.plugins[0].url, format!("https://web.{ns}.svc:7644"));
    // the entry's revision is the pod's
    let d = Api::<Deployment>::namespaced(env.client.clone(), &ns)
        .get("balerix-plugin-web")
        .await
        .unwrap();
    let env_rev = d.spec.unwrap().template.spec.unwrap().containers[0]
        .env
        .clone()
        .unwrap()
        .into_iter()
        .find(|e| e.name == "BALERIX_PLUGIN_REVISION")
        .unwrap()
        .value
        .unwrap();
    assert_eq!(list.plugins[0].revision, env_rev);
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn plugins_ready_follows_the_daemons_rows() {
    let (env, ns, stub, _clock, operator) = world("rows", &["web"]).await;
    let daemons: Api<Daemon> = Api::namespaced(env.client.clone(), &ns);
    let reason = || async {
        let d = daemons.get("default").await.unwrap();
        let s = d.status?;
        let c = s.conditions.iter().find(|c| c.type_ == "PluginsReady")?;
        Some((c.status.clone(), c.reason.clone(), c.message.clone()))
    };
    wait_for("PluginMissing", Duration::from_secs(60), || async {
        reason()
            .await
            .filter(|r| r.1 == "PluginMissing" && r.2 == "Plugin web does not exist")
    })
    .await;
    Api::<Plugin>::namespaced(env.client.clone(), &ns)
        .create(
            &PostParams::default(),
            &Plugin::new("web", plugin_spec(serde_json::json!({}))),
        )
        .await
        .unwrap();
    stub.set_plugin_rows(vec![PluginStatus {
        phase: AgentPhase::Failed,
        message: "hello.manifest.needs: kv is not granted".into(),
        ..ready_row("web")
    }]);
    wait_for("PluginRefused", Duration::from_secs(60), || async {
        reason().await.filter(|r| {
            r.1 == "PluginRefused" && r.2 == "web: hello.manifest.needs: kv is not granted"
        })
    })
    .await;
    // Ready says why too
    let d = daemons.get("default").await.unwrap();
    assert!(
        d.status
            .unwrap()
            .conditions
            .iter()
            .any(|c| c.type_ == "Ready" && c.reason == "PluginRefused")
    );
    stub.set_plugin_rows(vec![ready_row("web")]);
    wait_for("AllReady and Ready", Duration::from_secs(60), || async {
        let d = daemons.get("default").await.unwrap();
        let s = d.status?;
        (s.conditions
            .iter()
            .any(|c| c.type_ == "PluginsReady" && c.reason == "AllReady")
            && s.conditions
                .iter()
                .any(|c| c.type_ == "Ready" && c.status == "True"))
        .then_some(())
    })
    .await;
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_whose_config_cannot_be_built_holds_the_list_back() {
    let (env, ns, stub, _clock, operator) = world("heldback", &["flow", "web"]).await;
    let plugins: Api<Plugin> = Api::namespaced(env.client.clone(), &ns);
    plugins
        .create(
            &PostParams::default(),
            &Plugin::new("flow", plugin_spec(serde_json::json!({}))),
        )
        .await
        .unwrap();
    plugins
        .create(
            &PostParams::default(),
            &Plugin::new(
                "web",
                plugin_spec(serde_json::json!({ "needs": ["actions", "fleets", "manage"] })),
            ),
        )
        .await
        .unwrap();
    stub.set_managed(vec![managed("m", "web", None)]);
    wait_for("a list of both", Duration::from_secs(60), || async {
        made(&stub)
            .iter()
            .any(|l| l == &["flow", "web"])
            .then_some(())
    })
    .await;
    let fleets: Api<balerix_operator::api::Fleet> = Api::namespaced(env.client.clone(), &ns);
    wait_for("web's Fleet m", Duration::from_secs(60), || async {
        fleets.get_opt("m").await.unwrap()
    })
    .await;
    // web's config now names a Secret that does not exist
    plugins
        .patch(
            "web",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": {
        "secrets": { "password": { "secretName": "absent", "key": "pw" } } } })),
        )
        .await
        .unwrap();
    let daemons: Api<Daemon> = Api::namespaced(env.client.clone(), &ns);
    wait_for("PluginsReady says why", Duration::from_secs(60), || async {
        let s = daemons.get("default").await.unwrap().status?;
        s.conditions
            .iter()
            .find(|c| {
                c.type_ == "PluginsReady"
                    && c.reason == "SecretMissing"
                    && c.message == "web: spec.secrets.password: Secret absent does not exist"
            })
            .cloned()
    })
    .await;
    // no list without web went out: the Daemon keeps the last one, and with it web's managed requests
    let sent = stub.lists().len();
    support::hold_for(
        "a list sent while web is blocked",
        Duration::from_secs(3),
        || async { (stub.lists().len() > sent).then_some(()) },
    )
    .await;
    // from the first list of both on: one sent before flow's token and
    // certificate were made leaves flow out, as it should
    let lists = made(&stub);
    let both = lists.iter().position(|l| l == &["flow", "web"]).unwrap();
    assert!(
        lists[both..].iter().all(|l| l == &["flow", "web"]),
        "{lists:?}"
    );
    // and nothing touched web's Fleet
    let m = fleets.get("m").await.unwrap();
    assert!(m.metadata.deletion_timestamp.is_none());
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_whose_secrets_are_not_made_is_left_out_of_the_list() {
    // §23.4: a plugin that cannot be sent yet and has no managed requests
    // to lose is left out, and the rest go. The Plugin controller makes a
    // token and certificate at once, so envtest cannot hold one back; a
    // listed name with no Plugin object takes the same `continue`.
    let (env, ns, stub, _clock, operator) = world("leftout", &["flow", "ghost"]).await;
    Api::<Plugin>::namespaced(env.client.clone(), &ns)
        .create(
            &PostParams::default(),
            &Plugin::new("flow", plugin_spec(serde_json::json!({}))),
        )
        .await
        .unwrap();
    wait_for(
        "a list with flow alone",
        Duration::from_secs(60),
        || async { made(&stub).iter().any(|l| l == &["flow"]).then_some(()) },
    )
    .await;
    operator.abort();
}

fn managed(
    name: &str,
    plugin: &str,
    down: Option<balerix_api::DownQuery>,
) -> balerix_api::ManagedFleet {
    serde_json::from_value(serde_json::json!({ "name": name, "plugin": plugin, "file": {
        "apiVersion": "balerix/v1", "kind": "Fleet", "name": name,
        "crews": { "c": { "repo": "acme/api", "git": { "auth": "none" }, "agents": { "carol": {} } } } },
        "down": down })).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_managed_request_becomes_a_labelled_fleet_and_its_down_deletes_it() {
    let (env, ns, stub, _clock, operator) = world("managed", &["fake"]).await;
    let c = env.client.clone();
    Api::<Plugin>::namespaced(c.clone(), &ns)
        .create(
            &PostParams::default(),
            &Plugin::new(
                "fake",
                plugin_spec(serde_json::json!({ "needs": ["actions", "fleets", "kv", "manage"] })),
            ),
        )
        .await
        .unwrap();
    stub.set_managed(vec![managed("m", "fake", None)]);
    let fleets: Api<balerix_operator::api::Fleet> = Api::namespaced(c.clone(), &ns);
    let f = wait_for("Fleet m", Duration::from_secs(60), || async {
        fleets.get_opt("m").await.unwrap()
    })
    .await;
    assert_eq!(f.labels()["balerix.ai/managed-by"], "fake");
    assert_eq!(f.spec.daemon, "default");
    // the Fleet controller applies it for the plugin
    wait_for("a PUT managed by fake", Duration::from_secs(60), || async {
        stub.puts()
            .iter()
            .any(|p| p.spec.name == "m" && p.managed_by.as_deref() == Some("fake"))
            .then_some(())
    })
    .await;
    // the plugin downs it with keep-repos: Branches, then gone
    stub.set_managed(vec![managed(
        "m",
        "fake",
        Some(balerix_api::DownQuery {
            keep_repos: true,
            ..Default::default()
        }),
    )]);
    wait_for("Fleet m gone", Duration::from_secs(120), || async {
        fleets.get_opt("m").await.unwrap().is_none().then_some(())
    })
    .await;
    assert!(stub.deletes().contains(&"m".to_string()));
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_managed_request_never_overwrites_a_fleet_it_does_not_manage() {
    let (env, ns, stub, _clock, operator) = world("conflict", &["fake"]).await;
    let c = env.client.clone();
    Api::<Plugin>::namespaced(c.clone(), &ns)
        .create(
            &PostParams::default(),
            &Plugin::new(
                "fake",
                plugin_spec(serde_json::json!({ "needs": ["fleets", "manage"] })),
            ),
        )
        .await
        .unwrap();
    let fleets: Api<balerix_operator::api::Fleet> = Api::namespaced(c.clone(), &ns);
    let mine: balerix_operator::api::Fleet = serde_json::from_value(serde_json::json!({
        "apiVersion": "balerix.ai/v1alpha1", "kind": "Fleet", "metadata": { "name": "m" },
        "spec": { "daemon": "default", "crews": { "c": { "repo": "acme/mine", "git": { "auth": "none" }, "agents": { "me": {} } } } } })).unwrap();
    fleets.create(&PostParams::default(), &mine).await.unwrap();
    stub.set_managed(vec![managed("m", "fake", None)]);
    let events: Api<k8s_openapi::api::events::v1::Event> = Api::namespaced(c.clone(), &ns);
    wait_for(
        "FleetConflict on the Plugin",
        Duration::from_secs(60),
        || async {
            events
                .list(&Default::default())
                .await
                .unwrap()
                .items
                .into_iter()
                .find(|e| {
                    e.reason.as_deref() == Some("FleetConflict")
                        && e.regarding.as_ref().and_then(|r| r.name.as_deref()) == Some("fake")
                })
        },
    )
    .await;
    let still = fleets.get("m").await.unwrap();
    assert_eq!(still.spec.crews["c"].repo, "acme/mine");
    assert!(still.labels().get("balerix.ai/managed-by").is_none());
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_managed_request_never_repoints_another_daemons_fleet() {
    let (env, ns, stub, _clock, operator) = world("otherdaemon", &["fake"]).await;
    let c = env.client.clone();
    Api::<Plugin>::namespaced(c.clone(), &ns)
        .create(
            &PostParams::default(),
            &Plugin::new(
                "fake",
                plugin_spec(serde_json::json!({ "needs": ["fleets", "manage"] })),
            ),
        )
        .await
        .unwrap();
    // labelled for this plugin, but another Daemon's
    let fleets: Api<balerix_operator::api::Fleet> = Api::namespaced(c.clone(), &ns);
    let theirs: balerix_operator::api::Fleet = serde_json::from_value(serde_json::json!({
        "apiVersion": "balerix.ai/v1alpha1", "kind": "Fleet",
        "metadata": { "name": "m", "labels": { "balerix.ai/managed-by": "fake" } },
        "spec": { "daemon": "other", "crews": { "c": { "repo": "acme/theirs", "git": { "auth": "none" }, "agents": { "them": {} } } } } })).unwrap();
    fleets
        .create(&PostParams::default(), &theirs)
        .await
        .unwrap();
    stub.set_managed(vec![managed("m", "fake", None)]);
    let events: Api<k8s_openapi::api::events::v1::Event> = Api::namespaced(c.clone(), &ns);
    let event = wait_for(
        "FleetConflict on the Plugin",
        Duration::from_secs(60),
        || async {
            events
                .list(&Default::default())
                .await
                .unwrap()
                .items
                .into_iter()
                .find(|e| {
                    e.reason.as_deref() == Some("FleetConflict")
                        && e.regarding.as_ref().and_then(|r| r.name.as_deref()) == Some("fake")
                })
        },
    )
    .await;
    assert_eq!(
        event.note.as_deref(),
        Some("Fleet m belongs to Daemon other, not default: left as it is")
    );
    let still = fleets.get("m").await.unwrap();
    assert_eq!(still.spec.daemon, "other");
    assert_eq!(still.spec.crews["c"].repo, "acme/theirs");
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_plugin_deletes_its_fleets() {
    let (env, ns, stub, _clock, operator) = world("drop", &["fake"]).await;
    let c = env.client.clone();
    Api::<Plugin>::namespaced(c.clone(), &ns)
        .create(
            &PostParams::default(),
            &Plugin::new(
                "fake",
                plugin_spec(serde_json::json!({ "needs": ["fleets", "manage"] })),
            ),
        )
        .await
        .unwrap();
    stub.set_managed(vec![managed("m", "fake", None)]);
    let fleets: Api<balerix_operator::api::Fleet> = Api::namespaced(c.clone(), &ns);
    wait_for("Fleet m", Duration::from_secs(60), || async {
        fleets.get_opt("m").await.unwrap()
    })
    .await;
    Api::<Daemon>::namespaced(c.clone(), &ns)
        .patch(
            "default",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "plugins": [] } })),
        )
        .await
        .unwrap();
    // the Fleet's finalizer runs its crew's remove Job (`retain: None`)
    // once the crew's sync no longer holds it (§22.3); play the kubelet
    // for both
    let jobs: Api<k8s_openapi::api::batch::v1::Job> = Api::namespaced(c.clone(), &ns);
    wait_for("Job m-c-sync", Duration::from_secs(60), || async {
        jobs.get_opt("m-c-sync").await.unwrap()
    })
    .await;
    support::finish_job(&c, &ns, "m-c-sync", true, Some("0123abcd")).await;
    wait_for("Job m-c-remove", Duration::from_secs(60), || async {
        jobs.get_opt("m-c-remove").await.unwrap()
    })
    .await;
    support::finish_job(&c, &ns, "m-c-remove", true, None).await;
    wait_for("Fleet m gone", Duration::from_secs(120), || async {
        fleets.get_opt("m").await.unwrap().is_none().then_some(())
    })
    .await;
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_listed_twice_holds_the_list_back_and_keeps_its_fleets() {
    let (env, ns, stub, _clock, operator) = world("twicefleets", &["fake"]).await;
    let c = env.client.clone();
    Api::<Plugin>::namespaced(c.clone(), &ns)
        .create(
            &PostParams::default(),
            &Plugin::new(
                "fake",
                plugin_spec(serde_json::json!({ "needs": ["fleets", "manage"] })),
            ),
        )
        .await
        .unwrap();
    stub.set_managed(vec![managed("m", "fake", None)]);
    let fleets: Api<balerix_operator::api::Fleet> = Api::namespaced(c.clone(), &ns);
    wait_for("Fleet m", Duration::from_secs(60), || async {
        fleets.get_opt("m").await.unwrap()
    })
    .await;
    // a second Daemon lists it too (it never becomes ready: only the first talks to the stub)
    Api::<Daemon>::namespaced(c.clone(), &ns)
        .create(
            &PostParams::default(),
            &Daemon::new("other", daemon_spec(&["fake"])),
        )
        .await
        .unwrap();
    let daemons: Api<Daemon> = Api::namespaced(c.clone(), &ns);
    wait_for("PluginListedTwice", Duration::from_secs(60), || async {
        let s = daemons.get("default").await.unwrap().status?;
        s.conditions
            .iter()
            .find(|c| {
                c.type_ == "PluginsReady"
                    && c.reason == "PluginListedTwice"
                    && c.message == "Plugin fake is listed by another Daemon too"
            })
            .cloned()
    })
    .await;
    // no list without fake went out, so the Daemon keeps its managed requests
    let sent = stub.lists().len();
    support::hold_for(
        "a list sent while fake is listed twice",
        Duration::from_secs(3),
        || async { (stub.lists().len() > sent).then_some(()) },
    )
    .await;
    let m = fleets.get("m").await.unwrap();
    assert!(m.metadata.deletion_timestamp.is_none());
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_managed_request_is_an_event_and_the_rest_go_on() {
    let (env, ns, stub, _clock, operator) = world("refused", &["fake"]).await;
    let c = env.client.clone();
    Api::<Plugin>::namespaced(c.clone(), &ns)
        .create(
            &PostParams::default(),
            &Plugin::new(
                "fake",
                plugin_spec(serde_json::json!({ "needs": ["fleets", "manage"] })),
            ),
        )
        .await
        .unwrap();
    // the API server refuses the first (not a valid object name); the second is fine
    stub.set_managed(vec![
        managed("Bad_Name", "fake", None),
        managed("good", "fake", None),
    ]);
    stub.set_plugin_rows(vec![ready_row("fake")]);
    let events: Api<k8s_openapi::api::events::v1::Event> = Api::namespaced(c.clone(), &ns);
    let event = wait_for(
        "ManagedFleetRefused on the Plugin",
        Duration::from_secs(60),
        || async {
            events
                .list(&Default::default())
                .await
                .unwrap()
                .items
                .into_iter()
                .find(|e| {
                    e.reason.as_deref() == Some("ManagedFleetRefused")
                        && e.regarding.as_ref().and_then(|r| r.name.as_deref()) == Some("fake")
                })
        },
    )
    .await;
    assert!(
        event
            .note
            .as_deref()
            .unwrap_or_default()
            .starts_with("managed Fleet Bad_Name: "),
        "{:?}",
        event.note
    );
    let fleets: Api<balerix_operator::api::Fleet> = Api::namespaced(c.clone(), &ns);
    let good = wait_for("Fleet good", Duration::from_secs(60), || async {
        fleets.get_opt("good").await.unwrap()
    })
    .await;
    assert_eq!(good.labels()["balerix.ai/managed-by"], "fake");
    // the reconcile went on to write the Daemon's status
    let daemons: Api<Daemon> = Api::namespaced(c.clone(), &ns);
    wait_for("PluginsReady=AllReady", Duration::from_secs(60), || async {
        let s = daemons.get("default").await.unwrap().status?;
        s.conditions
            .iter()
            .any(|c| c.type_ == "PluginsReady" && c.reason == "AllReady")
            .then_some(())
    })
    .await;
    operator.abort();
}

/// The Daemon's `type_` condition: status, reason, message.
async fn daemon_condition(daemons: &Api<Daemon>, type_: &str) -> Option<(String, String, String)> {
    let s = daemons.get("default").await.unwrap().status?;
    let c = s.conditions.iter().find(|c| c.type_ == type_)?;
    Some((c.status.clone(), c.reason.clone(), c.message.clone()))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_name_a_service_cannot_take_is_refused_before_anything_is_made() {
    // a valid object name, but not a Service's (DNS-1035 starts with a letter)
    let (env, ns, stub, _clock, operator) = world("badname", &["1password"]).await;
    let c = env.client.clone();
    let plugins: Api<Plugin> = Api::namespaced(c.clone(), &ns);
    plugins
        .create(
            &PostParams::default(),
            &Plugin::new("1password", plugin_spec(serde_json::json!({}))),
        )
        .await
        .unwrap();
    let why = "metadata.name: starts with a digit; its Service's name must start with a letter";
    let got = wait_for(
        "Deployed=False/InvalidSpec",
        Duration::from_secs(60),
        || async {
            condition(&plugins.get("1password").await.unwrap(), "Deployed")
                .filter(|c| c.1 == "InvalidSpec")
        },
    )
    .await;
    assert_eq!(got.2, why);
    // the Daemon holds its list back and still writes its status
    let daemons: Api<Daemon> = Api::namespaced(c.clone(), &ns);
    let got = wait_for(
        "PluginsReady=False/InvalidSpec",
        Duration::from_secs(60),
        || async {
            daemon_condition(&daemons, "PluginsReady")
                .await
                .filter(|c| c.1 == "InvalidSpec")
        },
    )
    .await;
    assert_eq!(
        (got.0.as_str(), got.2.as_str()),
        ("False", format!("1password: {why}").as_str())
    );
    assert!(
        made(&stub)
            .iter()
            .all(|l| !l.contains(&"1password".to_string())),
        "{:?}",
        made(&stub)
    );
    // nothing was made for it
    assert!(
        Api::<Deployment>::namespaced(c.clone(), &ns)
            .get_opt("balerix-plugin-1password")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        Api::<Service>::namespaced(c.clone(), &ns)
            .get_opt("1password")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        Api::<Secret>::namespaced(c.clone(), &ns)
            .get_opt("balerix-plugin-1password-token")
            .await
            .unwrap()
            .is_none()
    );
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_list_the_daemon_refuses_is_list_refused() {
    let (env, ns, stub, _clock, operator) = world("listrefused", &["web"]).await;
    stub.reject_lists(Some("plugins[0].grant: not a capability"));
    Api::<Plugin>::namespaced(env.client.clone(), &ns)
        .create(
            &PostParams::default(),
            &Plugin::new("web", plugin_spec(serde_json::json!({}))),
        )
        .await
        .unwrap();
    let daemons: Api<Daemon> = Api::namespaced(env.client.clone(), &ns);
    let got = wait_for(
        "PluginsReady=False/ListRefused",
        Duration::from_secs(60),
        || async {
            daemon_condition(&daemons, "PluginsReady")
                .await
                .filter(|c| c.1 == "ListRefused")
        },
    )
    .await;
    assert_eq!(
        (got.0.as_str(), got.2.as_str()),
        ("False", "plugins[0].grant: not a capability")
    );
    let ready = daemon_condition(&daemons, "Ready").await.unwrap();
    assert_eq!(
        (ready.0.as_str(), ready.1.as_str()),
        ("False", "ListRefused")
    );
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secret_the_operator_manages_is_never_a_plugins_config() {
    let (env, ns, stub, _clock, operator) = world("opsecret", &["web"]).await;
    let c = env.client.clone();
    let plugins: Api<Plugin> = Api::namespaced(c.clone(), &ns);
    plugins
        .create(
            &PostParams::default(),
            &Plugin::new("web", plugin_spec(serde_json::json!({}))),
        )
        .await
        .unwrap();
    // sent once, so the Daemon judges web's config rather than leaving it
    // out for want of a token
    wait_for("a list with web", Duration::from_secs(60), || async {
        made(&stub).iter().any(|l| l == &["web"]).then_some(())
    })
    .await;
    plugins
        .patch(
            "web",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": {
        "secrets": { "key": { "secretName": "balerix-default-ca", "key": "ca.key" } } } })),
        )
        .await
        .unwrap();
    let why = "spec.secrets.key: Secret balerix-default-ca is managed by the operator";
    let got = wait_for(
        "Deployed=False/InvalidSpec",
        Duration::from_secs(60),
        || async {
            condition(&plugins.get("web").await.unwrap(), "Deployed")
                .filter(|c| c.1 == "InvalidSpec")
        },
    )
    .await;
    assert_eq!(got.2, why);
    let daemons: Api<Daemon> = Api::namespaced(c.clone(), &ns);
    let got = wait_for(
        "PluginsReady=False/InvalidSpec",
        Duration::from_secs(60),
        || async {
            daemon_condition(&daemons, "PluginsReady")
                .await
                .filter(|c| c.1 == "InvalidSpec")
        },
    )
    .await;
    assert_eq!(got.2, format!("web: {why}"));
    // the key itself is nowhere: no condition, no Event, no list
    let key = Api::<Secret>::namespaced(c.clone(), &ns)
        .get("balerix-default-ca")
        .await
        .unwrap()
        .data
        .unwrap()["ca.key"]
        .0
        .clone();
    let key = String::from_utf8(key).unwrap();
    let body = key
        .lines()
        .find(|l| !l.starts_with("-----") && !l.is_empty())
        .unwrap()
        .to_string();
    let plugin = serde_json::to_string(&plugins.get("web").await.unwrap()).unwrap();
    let daemon = serde_json::to_string(&daemons.get("default").await.unwrap()).unwrap();
    let events = serde_json::to_string(
        &Api::<k8s_openapi::api::events::v1::Event>::namespaced(c.clone(), &ns)
            .list(&Default::default())
            .await
            .unwrap()
            .items,
    )
    .unwrap();
    let lists = serde_json::to_string(&stub.lists()).unwrap();
    for (what, text) in [
        ("Plugin", plugin),
        ("Daemon", daemon),
        ("Events", events),
        ("lists", lists),
    ] {
        assert!(!text.contains(&body), "the CA key in the {what}");
    }
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_plugins_remade_token_holds_the_list_back() {
    let (env, ns, stub, _clock, operator) = world("remade", &["fake"]).await;
    let c = env.client.clone();
    let plugins: Api<Plugin> = Api::namespaced(c.clone(), &ns);
    plugins
        .create(
            &PostParams::default(),
            &Plugin::new(
                "fake",
                plugin_spec(serde_json::json!({ "needs": ["fleets", "manage"] })),
            ),
        )
        .await
        .unwrap();
    stub.set_managed(vec![managed("m", "fake", None)]);
    stub.set_plugin_rows(vec![ready_row("fake")]);
    let fleets: Api<balerix_operator::api::Fleet> = Api::namespaced(c.clone(), &ns);
    wait_for("Fleet m", Duration::from_secs(60), || async {
        fleets.get_opt("m").await.unwrap()
    })
    .await;
    // the Plugin controller cannot remake the token while fake's config is
    // blocked, so the Daemon controller sees it missing for a running plugin
    plugins
        .patch(
            "fake",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": {
        "secrets": { "password": { "secretName": "absent", "key": "pw" } } } })),
        )
        .await
        .unwrap();
    let daemons: Api<Daemon> = Api::namespaced(c.clone(), &ns);
    wait_for(
        "PluginsReady=False/SecretMissing",
        Duration::from_secs(60),
        || async {
            daemon_condition(&daemons, "PluginsReady")
                .await
                .filter(|c| c.1 == "SecretMissing")
        },
    )
    .await;
    let before = stub.lists().len();
    Api::<Secret>::namespaced(c.clone(), &ns)
        .delete("balerix-plugin-fake-token", &DeleteParams::default())
        .await
        .unwrap();
    let got = wait_for(
        "PluginsReady=False/PluginNotReady",
        Duration::from_secs(60),
        || async {
            daemon_condition(&daemons, "PluginsReady")
                .await
                .filter(|c| c.1 == "PluginNotReady")
        },
    )
    .await;
    assert_eq!(got.2, "fake: its token or serving Secret is being remade");
    // no list without fake went out, so the Daemon keeps its managed requests
    let sent = stub.lists().len();
    support::hold_for(
        "a list sent while fake's token is being remade",
        Duration::from_secs(3),
        || async { (stub.lists().len() > sent).then_some(()) },
    )
    .await;
    let lists = made(&stub);
    assert!(lists[before..].iter().all(|l| l == &["fake"]), "{lists:?}");
    let m = fleets.get("m").await.unwrap();
    assert!(m.metadata.deletion_timestamp.is_none());
    operator.abort();
}
