#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The charts (Spec O §14.1, §24.1–§24.3): `helm template` renderings
//! asserted, applied in a server-side dry run against a real API server,
//! and the controllers run as the operator chart's service account, so a
//! verb its RBAC lacks is a 403 here. Needs `helm` (skips without it,
//! fails under `BALERIX_REQUIRE_TOOLS=1`); the API-server tests need the
//! envtest binaries too. `mise run charts` runs this file.
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::{Value, json};

fn helm_ok() -> bool {
    let ok = Command::new("helm")
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        assert!(
            std::env::var("BALERIX_REQUIRE_TOOLS").as_deref() != Ok("1"),
            "helm is required (BALERIX_REQUIRE_TOOLS=1); run `mise run charts`"
        );
        eprintln!("skip: no helm");
    }
    ok
}

fn chart(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../charts")
        .join(name)
}

/// `helm template <release> <chart> -n <namespace> -f <values>`, one JSON
/// value per non-empty document.
fn render(chart: &Path, release: &str, namespace: &str, values: Value) -> Vec<Value> {
    let file = std::env::temp_dir().join(format!(
        "charts-it-{}-{}.json",
        std::process::id(),
        support::now_nanos()
    ));
    std::fs::write(&file, serde_json::to_vec(&values).unwrap()).unwrap();
    let out = Command::new("helm")
        .args(["template", release])
        .arg(chart)
        .args(["--namespace", namespace, "-f"])
        .arg(&file)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&file);
    assert!(
        out.status.success(),
        "helm template failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .split("\n---")
        .filter_map(|doc| serde_norway::from_str::<Value>(doc).ok())
        .filter(|v| v.is_object())
        .collect()
}

fn find<'a>(docs: &'a [Value], kind: &str, name: &str) -> &'a Value {
    docs.iter()
        .find(|d| d["kind"] == kind && d["metadata"]["name"] == name)
        .unwrap_or_else(|| panic!("no {kind} {name} in {:?}", kinds(docs)))
}

fn kinds(docs: &[Value]) -> Vec<String> {
    docs.iter()
        .map(|d| {
            format!(
                "{}/{}",
                d["kind"].as_str().unwrap(),
                d["metadata"]["name"].as_str().unwrap()
            )
        })
        .collect()
}

fn operator(namespace: &str, values: Value) -> Vec<Value> {
    render(
        &chart("balerix-operator"),
        "balerix-operator",
        namespace,
        values,
    )
}

fn container(docs: &[Value]) -> &Value {
    &find(docs, "Deployment", "balerix-operator")["spec"]["template"]["spec"]["containers"][0]
}

fn args(docs: &[Value]) -> Vec<String> {
    serde_json::from_value(container(docs)["args"].clone()).unwrap()
}

#[test]
fn the_operator_pod_carries_the_label_the_daemons_policy_admits() {
    if !helm_ok() {
        return;
    }
    let docs = operator("balerix-system", json!({}));
    let d = find(&docs, "Deployment", "balerix-operator");
    assert_eq!(d["metadata"]["namespace"], "balerix-system");
    let labels = &d["spec"]["template"]["metadata"]["labels"];
    // desired::daemon's NetworkPolicy admits this label from the
    // operator's namespace (§10.2); kind does not enforce the policy, so
    // this is where the two are held together
    assert_eq!(
        labels["app.kubernetes.io/name"],
        balerix_operator::desired::common::MANAGER
    );
    let selector = &d["spec"]["selector"]["matchLabels"];
    for (k, v) in selector.as_object().unwrap() {
        assert_eq!(&labels[k], v, "the selector's {k} is on the pod");
    }
}

#[test]
fn one_replica_recreated_hardened_and_told_its_namespace() {
    if !helm_ok() {
        return;
    }
    let docs = operator("balerix-system", json!({}));
    let d = find(&docs, "Deployment", "balerix-operator");
    assert_eq!(d["spec"]["replicas"], 1);
    assert_eq!(d["spec"]["strategy"]["type"], "Recreate");
    let pod = &d["spec"]["template"]["spec"];
    assert_eq!(pod["serviceAccountName"], "balerix-operator");
    assert_eq!(pod["securityContext"]["runAsNonRoot"], true);
    assert_eq!(pod["securityContext"]["runAsUser"], 65532);
    assert_eq!(
        pod["securityContext"]["seccompProfile"]["type"],
        "RuntimeDefault"
    );
    let c = container(&docs);
    assert_eq!(c["image"], "ghcr.io/balerix-ai/balerix-operator:0.2.0");
    assert_eq!(c["securityContext"]["allowPrivilegeEscalation"], false);
    assert_eq!(c["securityContext"]["readOnlyRootFilesystem"], true);
    assert_eq!(c["securityContext"]["capabilities"]["drop"], json!(["ALL"]));
    assert!(c.get("livenessProbe").is_none() && c.get("readinessProbe").is_none());
    assert_eq!(args(&docs), ["run"]);
    let env = c["env"].as_array().unwrap();
    let pod_ns = env.iter().find(|e| e["name"] == "POD_NAMESPACE").unwrap();
    assert_eq!(
        pod_ns["valueFrom"]["fieldRef"]["fieldPath"],
        "metadata.namespace"
    );
    let log = env.iter().find(|e| e["name"] == "RUST_LOG").unwrap();
    assert_eq!(log["value"], "info,kube=warn");
}

#[test]
fn the_default_is_one_cluster_role_named_for_the_namespace() {
    if !helm_ok() {
        return;
    }
    let docs = operator("staging", json!({}));
    let role = find(&docs, "ClusterRole", "staging-balerix-operator");
    let binding = find(&docs, "ClusterRoleBinding", "staging-balerix-operator");
    assert_eq!(binding["roleRef"]["name"], "staging-balerix-operator");
    assert_eq!(
        binding["subjects"],
        json!([{ "kind": "ServiceAccount", "name": "balerix-operator", "namespace": "staging" }])
    );
    assert!(
        !docs
            .iter()
            .any(|d| d["kind"] == "Role" || d["kind"] == "RoleBinding")
    );
    assert_rules(role);
}

#[test]
fn watch_namespaces_become_a_role_in_each_and_the_flag() {
    if !helm_ok() {
        return;
    }
    let docs = operator("balerix-system", json!({ "watchNamespaces": ["a", "b"] }));
    for ns in ["a", "b"] {
        let role = docs
            .iter()
            .find(|d| d["kind"] == "Role" && d["metadata"]["namespace"] == ns)
            .unwrap_or_else(|| panic!("no Role in {ns}"));
        assert_rules(role);
        let binding = docs
            .iter()
            .find(|d| d["kind"] == "RoleBinding" && d["metadata"]["namespace"] == ns)
            .unwrap_or_else(|| panic!("no RoleBinding in {ns}"));
        assert_eq!(binding["subjects"][0]["namespace"], "balerix-system");
    }
    assert!(!docs.iter().any(|d| d["kind"] == "ClusterRole"));
    assert_eq!(args(&docs), ["run", "--watch-namespaces", "a,b"]);
}

#[test]
fn images_override_the_operators_defaults() {
    if !helm_ok() {
        return;
    }
    let docs = operator(
        "balerix-system",
        json!({ "image": { "repository": "balerix-operator", "tag": "e2e" },
                "images": { "daemon": "balerix:e2e", "agent": "balerix-agent:e2e" } }),
    );
    assert_eq!(container(&docs)["image"], "balerix-operator:e2e");
    assert_eq!(
        args(&docs),
        [
            "run",
            "--daemon-image",
            "balerix:e2e",
            "--agent-image",
            "balerix-agent:e2e"
        ]
    );
}

#[test]
fn crds_install_false_renders_no_definition() {
    if !helm_ok() {
        return;
    }
    let with = operator("balerix-system", json!({}));
    assert_eq!(
        with.iter()
            .filter(|d| d["kind"] == "CustomResourceDefinition")
            .count(),
        5
    );
    let without = operator("balerix-system", json!({ "crds": { "install": false } }));
    assert!(
        !without
            .iter()
            .any(|d| d["kind"] == "CustomResourceDefinition")
    );
}

/// The table in the plan's Global Constraints, which is §24.1's list.
fn assert_rules(role: &Value) {
    let rules = role["rules"].as_array().unwrap();
    let grants = |group: &str, resource: &str, verb: &str| {
        rules.iter().any(|r| {
            r["apiGroups"]
                .as_array()
                .unwrap()
                .iter()
                .any(|g| g == group)
                && r["resources"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|x| x == resource)
                && r["verbs"].as_array().unwrap().iter().any(|v| v == verb)
        })
    };
    for kind in ["daemons", "fleets", "crews", "agents", "plugins"] {
        for verb in [
            "get", "list", "watch", "create", "update", "patch", "delete",
        ] {
            assert!(grants("balerix.ai", kind, verb), "{kind} {verb}");
        }
        assert!(grants("balerix.ai", &format!("{kind}/status"), "patch"));
        assert!(grants(
            "balerix.ai",
            &format!("{kind}/finalizers"),
            "update"
        ));
    }
    for (group, resource) in [
        ("", "secrets"),
        ("", "services"),
        ("", "configmaps"),
        ("", "persistentvolumeclaims"),
        ("", "pods"),
        ("apps", "statefulsets"),
        ("apps", "deployments"),
        ("batch", "jobs"),
        ("networking.k8s.io", "networkpolicies"),
    ] {
        assert!(grants(group, resource, "delete"), "{group}/{resource}");
    }
    assert!(grants("events.k8s.io", "events", "create"));
    assert!(grants("events.k8s.io", "events", "patch"));
    assert!(!grants(
        "apiextensions.k8s.io",
        "customresourcedefinitions",
        "get"
    ));
}

use balerix_operator::api::{Daemon, Fleet, Plugin};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Pod, Secret};
use kube::api::{DynamicObject, GroupVersionKind, Patch, PatchParams, PostParams};
use kube::discovery::Discovery;
use kube::{Api, Client};
use support::envtest::{EnvTest, envtest};
use support::stub_daemon::StubDaemon;
use support::{finish_job, make_daemon_ready, namespace, wait_for};

/// Server-side apply of each document, as a dry run unless `persist`.
/// Force: the definitions are already there, created by the harness under
/// another field manager.
async fn apply_docs(client: &Client, docs: &[Value], persist: bool) {
    let discovery = Discovery::new(client.clone()).run().await.unwrap();
    for doc in docs {
        let (group, version) = match doc["apiVersion"].as_str().unwrap().split_once('/') {
            Some((g, v)) => (g.to_string(), v.to_string()),
            None => (
                String::new(),
                doc["apiVersion"].as_str().unwrap().to_string(),
            ),
        };
        let gvk = GroupVersionKind::gvk(&group, &version, doc["kind"].as_str().unwrap());
        let (resource, caps) = discovery
            .resolve_gvk(&gvk)
            .unwrap_or_else(|| panic!("the API server serves no {gvk:?}"));
        let api: Api<DynamicObject> = match (caps.scope, doc["metadata"]["namespace"].as_str()) {
            (kube::discovery::Scope::Namespaced, Some(ns)) => {
                Api::namespaced_with(client.clone(), ns, &resource)
            }
            _ => Api::all_with(client.clone(), &resource),
        };
        let mut params = PatchParams::apply("charts-it").force();
        if !persist {
            params = params.dry_run();
        }
        let name = doc["metadata"]["name"].as_str().unwrap();
        api.patch(name, &params, &Patch::Apply(doc))
            .await
            .unwrap_or_else(|e| panic!("{}/{name}: {e}", doc["kind"]));
    }
}

async fn dry_run_apply(client: &Client, docs: &[Value]) {
    apply_docs(client, docs, false).await;
}

async fn apply(client: &Client, doc: &Value) {
    apply_docs(client, std::slice::from_ref(doc), true).await;
}

/// The operator chart's value sets §24.3 names: the defaults and Roles
/// in two namespaces (the daemon chart's are Task 3's test).
#[tokio::test(flavor = "multi_thread")]
async fn every_rendering_is_accepted_by_the_api_server() {
    if !helm_ok() {
        return;
    }
    let Some(env) = envtest().await else { return };
    let op = namespace(&env.client, "op").await;
    let a = namespace(&env.client, "a").await;
    let b = namespace(&env.client, "b").await;
    dry_run_apply(&env.client, &operator(&op, json!({}))).await;
    dry_run_apply(
        &env.client,
        &operator(&op, json!({ "watchNamespaces": [a, b] })),
    )
    .await;
}

/// The operator's two clients, as the chart's service account.
async fn as_service_account(env: &EnvTest, namespace: &str, name: &str) -> (Client, Client) {
    let kc = kube::config::Kubeconfig::read_from(&env.kubeconfig).unwrap();
    let mut config =
        kube::Config::from_custom_kubeconfig(kc, &kube::config::KubeConfigOptions::default())
            .await
            .unwrap();
    config.auth_info.impersonate = Some(format!("system:serviceaccount:{namespace}:{name}"));
    config.auth_info.impersonate_groups = Some(vec![
        "system:serviceaccounts".into(),
        format!("system:serviceaccounts:{namespace}"),
        "system:authenticated".into(),
    ]);
    (
        balerix_operator::request_client::request_client(config.clone()).unwrap(),
        balerix_operator::watch_client::watch_client(config).unwrap(),
    )
}

/// The chart's ServiceAccount and RBAC applied for real (not its
/// Deployment: no kubelet runs it), then the controllers in this process
/// under that account: a Daemon listing a Plugin, and a Fleet down to an
/// Agent's pod. The test is the kubelet, as in controllers_it.
async fn rbac_journey(label: &str, namespaced: bool) {
    if !helm_ok() {
        return;
    }
    let Some(env) = envtest().await else { return };
    let admin = env.client.clone();
    let op = namespace(&admin, &format!("{label}-op")).await;
    let ns = namespace(&admin, label).await;
    let mut values = json!({ "crds": { "install": false } });
    if namespaced {
        values["watchNamespaces"] = json!([ns]);
    }
    for doc in operator(&op, values)
        .iter()
        .filter(|d| d["kind"] != "Deployment")
    {
        apply(&admin, doc).await;
    }
    let (client, watches) = as_service_account(env, &op, "balerix-operator").await;

    let stub = StubDaemon::start().await;
    let mut cfg = balerix_operator::controllers::RunConfig::new(
        "0.2.0",
        balerix_operator::desired::common::Images {
            daemon: "balerix:test".into(),
            agent: "balerix-agent:test".into(),
        },
        &op,
    );
    cfg.watch_namespaces = namespaced.then(|| vec![ns.clone()]);
    cfg.fleet_period = Duration::from_secs(1);
    cfg.period = Duration::from_secs(1);
    cfg.insecure_daemon_url = Some(stub.url());
    let operator = tokio::spawn(balerix_operator::controllers::run(
        client.clone(),
        watches,
        cfg,
    ))
    .abort_handle();

    // a Daemon listing flow, and flow's Plugin: StatefulSet, Secrets,
    // ConfigMap, Service, NetworkPolicy, claims, the pool Job, a Deployment
    let daemon: Daemon = serde_json::from_value(json!({
        "apiVersion": "balerix.ai/v1alpha1", "kind": "Daemon",
        "metadata": { "name": "default", "namespace": ns },
        "spec": { "storage": { "state": { "size": "1Gi" }, "shared": { "size": "10Gi" }, "agent": { "size": "2Gi" } },
                  "plugins": ["flow"] }
    }))
    .unwrap();
    Api::<Daemon>::namespaced(admin.clone(), &ns)
        .create(&PostParams::default(), &daemon)
        .await
        .unwrap();
    let plugin: Plugin = serde_json::from_value(json!({
        "apiVersion": "balerix.ai/v1alpha1", "kind": "Plugin",
        "metadata": { "name": "flow", "namespace": ns },
        "spec": { "image": "balerix-plugin-flow:test", "needs": ["actions", "kv"] }
    }))
    .unwrap();
    Api::<Plugin>::namespaced(admin.clone(), &ns)
        .create(&PostParams::default(), &plugin)
        .await
        .unwrap();
    make_daemon_ready(&admin, &ns).await;
    wait_for("flow's Deployment", Duration::from_secs(60), || async {
        Api::<Deployment>::namespaced(admin.clone(), &ns)
            .get_opt("balerix-plugin-flow")
            .await
            .unwrap()
    })
    .await;
    wait_for("the Daemon's status", Duration::from_secs(60), || async {
        Api::<Daemon>::namespaced(admin.clone(), &ns)
            .get("default")
            .await
            .unwrap()
            .status
            .filter(|s| !s.conditions.is_empty())
    })
    .await;

    // a Fleet: Crew, Agent, token Secret, the Jobs, the claim, the pod
    let fleet: Fleet = serde_json::from_value(json!({
        "apiVersion": "balerix.ai/v1alpha1", "kind": "Fleet",
        "metadata": { "name": "f", "namespace": ns },
        "spec": { "daemon": "default", "retain": "Branches",
                  "crews": { "c": { "repo": "acme/api", "git": { "auth": "none" }, "agents": { "a": {} } } } }
    }))
    .unwrap();
    Api::<Fleet>::namespaced(admin.clone(), &ns)
        .create(&PostParams::default(), &fleet)
        .await
        .unwrap();
    let jobs: Api<Job> = Api::namespaced(admin.clone(), &ns);
    wait_for("the Fleet's Jobs", Duration::from_secs(60), || async {
        jobs.get_opt("f-pool").await.unwrap()?;
        jobs.get_opt("f-c-sync").await.unwrap()
    })
    .await;
    finish_job(&admin, &ns, "f-pool", true, Some("synced")).await;
    finish_job(&admin, &ns, "f-c-sync", true, Some("0123abcd")).await;
    wait_for("Agent f-c-a's pod", Duration::from_secs(60), || async {
        Api::<Pod>::namespaced(admin.clone(), &ns)
            .get_opt("f-c-a")
            .await
            .unwrap()
    })
    .await;

    // Events (§22.5), as the operator's Recorder writes them
    let event: k8s_openapi::api::events::v1::Event = serde_json::from_value(json!({
        "metadata": { "name": "charts-it", "namespace": ns },
        "eventTime": "2026-10-06T00:00:00.000000Z",
        "reportingController": "balerix-operator", "reportingInstance": "charts-it",
        "action": "Reconcile", "reason": "ChartsIt", "type": "Normal",
        "regarding": { "kind": "Fleet", "name": "f", "namespace": ns }
    }))
    .unwrap();
    Api::<k8s_openapi::api::events::v1::Event>::namespaced(client.clone(), &ns)
        .create(&PostParams::default(), &event)
        .await
        .expect("the chart grants Events");

    if namespaced {
        // Roles in the watched namespaces only: nothing outside them
        let outside = Api::<Secret>::namespaced(client.clone(), &op)
            .list(&Default::default())
            .await
            .unwrap_err();
        assert!(
            matches!(&outside, kube::Error::Api(s) if s.code == 403),
            "a Secret outside the watched namespaces: {outside}"
        );
    }
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_operator_runs_under_the_charts_namespaced_rbac() {
    rbac_journey("rbac-ns", true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_operator_runs_under_the_charts_cluster_rbac() {
    rbac_journey("rbac-cluster", false).await;
}

fn daemon_chart(release: &str, values: Value) -> Vec<Value> {
    render(&chart("balerix-daemon"), release, "team-a", values)
}

#[test]
fn the_daemon_is_named_for_the_release_and_carries_the_values() {
    if !helm_ok() {
        return;
    }
    let docs = daemon_chart(
        "payments",
        json!({
            "storage": { "shared": { "size": "50Gi", "storageClassName": "nfs" } },
            "credentials": { "claude": { "secretName": "claude-creds" } },
            "defaults": { "claude": { "settings": { "model": "sonnet" } } },
            "resources": { "requests": { "cpu": "500m" } }
        }),
    );
    assert_eq!(kinds(&docs), ["Daemon/payments"]);
    let d: Daemon = serde_json::from_value(find(&docs, "Daemon", "payments").clone()).unwrap();
    assert_eq!(d.metadata.namespace.as_deref(), Some("team-a"));
    let want: balerix_operator::api::DaemonSpec = serde_json::from_value(json!({
        "storage": { "state": { "size": "1Gi" },
                     "shared": { "size": "50Gi", "storageClassName": "nfs" },
                     "agent": { "size": "2Gi" } },
        "credentials": { "claude": { "secretName": "claude-creds" } },
        "defaults": { "claude": { "settings": { "model": "sonnet" } } },
        "plugins": [],
        "resources": { "requests": { "cpu": "500m" } }
    }))
    .unwrap();
    assert_eq!(d.spec, want);
    let named = daemon_chart("payments", json!({ "name": "default" }));
    find(&named, "Daemon", "default");
}

/// Plan Review Focus 5: an empty optional is absent, never "".
#[test]
fn empty_optionals_render_nothing() {
    if !helm_ok() {
        return;
    }
    let docs = daemon_chart("d", json!({ "plugins": { "web": { "enabled": true } } }));
    let d = find(&docs, "Daemon", "d");
    for claim in ["state", "shared", "agent"] {
        assert!(
            d["spec"]["storage"][claim]
                .get("storageClassName")
                .is_none(),
            "{claim}"
        );
    }
    assert!(d["spec"].get("credentials").is_none());
    assert!(d["spec"].get("version").is_none(), "the operator's version");
    let web = find(&docs, "Plugin", "web");
    assert!(web["spec"].get("expose").is_none());
    assert!(web["spec"].get("scratch").is_none());
}

#[test]
fn enabled_plugins_are_objects_listed_in_order_then_the_extra_ones() {
    if !helm_ok() {
        return;
    }
    let docs = daemon_chart(
        "d",
        json!({
            "plugins": {
                "web": { "enabled": true, "config": { "enabled": true } },
                "flow": { "enabled": true, "image": { "repository": "balerix-plugin-flow", "tag": "e2e" } },
                "github": { "enabled": true, "expose": { "port": 8080 },
                            "secrets": { "webhook_secret": { "secretName": "gh", "key": "secret" } } }
            },
            "extraPlugins": ["fake"]
        }),
    );
    assert_eq!(
        find(&docs, "Daemon", "d")["spec"]["plugins"],
        json!(["flow", "web", "github", "fake"])
    );
    assert!(
        !docs
            .iter()
            .any(|d| d["kind"] == "Plugin" && d["metadata"]["name"] == "matrix")
    );
    assert!(
        !docs
            .iter()
            .any(|d| d["kind"] == "Plugin" && d["metadata"]["name"] == "fake")
    );
    let flow: Plugin = serde_json::from_value(find(&docs, "Plugin", "flow").clone()).unwrap();
    assert_eq!(flow.spec.image, "balerix-plugin-flow:e2e");
    let web: Plugin = serde_json::from_value(find(&docs, "Plugin", "web").clone()).unwrap();
    assert_eq!(
        web.spec.image,
        "ghcr.io/balerix-ai/balerix-plugin-web:0.2.1"
    );
    assert_eq!(web.spec.config, json!({ "enabled": true }));
    let github: Plugin = serde_json::from_value(find(&docs, "Plugin", "github").clone()).unwrap();
    assert_eq!(github.spec.expose.unwrap().port, 8080);
    assert_eq!(github.spec.secrets["webhook_secret"].secret_name, "gh");
}

/// Plan Review Focus 2: the chart's grant is each plugin's manifest's.
#[test]
fn each_plugins_needs_are_its_manifests() {
    if !helm_ok() {
        return;
    }
    let all = json!({ "plugins": {
        "flow": { "enabled": true }, "web": { "enabled": true },
        "matrix": { "enabled": true }, "github": { "enabled": true } } });
    let docs = daemon_chart("d", all);
    for p in ["flow", "web", "matrix", "github"] {
        let manifest: Value = serde_norway::from_str(
            &std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(format!("../plugins/{p}/package/balerix-plugin.yaml")),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            find(&docs, "Plugin", p)["spec"]["needs"],
            manifest["needs"],
            "{p}: the chart's needs differ from its manifest's"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_chart_is_accepted_by_the_api_server() {
    if !helm_ok() {
        return;
    }
    let Some(env) = envtest().await else { return };
    let ns = namespace(&env.client, "daemon-chart").await;
    let all = json!({
        "storage": { "shared": { "storageClassName": "nfs" } },
        "credentials": { "claude": { "secretName": "c" }, "github": { "secretName": "g" } },
        "plugins": {
            "flow": { "enabled": true }, "web": { "enabled": true },
            "matrix": { "enabled": true }, "github": { "enabled": true, "expose": { "port": 8080 } } },
        "extraPlugins": ["fake"] });
    for values in [json!({}), all] {
        let docs = render(&chart("balerix-daemon"), "d", &ns, values);
        dry_run_apply(&env.client, &docs).await;
    }
}
