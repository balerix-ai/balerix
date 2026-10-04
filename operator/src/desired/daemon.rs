//! A Daemon object into what runs it (Spec O §5.1): two claims, a
//! StatefulSet of one `balerix serve --mode kubernetes`, its Service, its
//! NetworkPolicy and the Job that installs the daemon pool. The Secrets
//! come from material the controller minted (`pki`); nothing random is
//! made here.

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{
    ConfigMap, PersistentVolumeClaim, ResourceRequirements, Secret, Service,
};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use serde_json::{Value, json};

use super::common::{
    Cond, DAEMON_PORT, DesiredError, JobOutcome, MANAGER, OperatorConfig, claim, conditions,
    container_security, labels, owner_of, pod_security, typed,
};
use super::jobs::{JobContext, daemon_pool_job};
use super::names;
use crate::api::{Daemon, DaemonStatus};
use crate::pki::Issued;

/// On a certificate's Secret: when it expires, unix seconds. On the
/// daemon's pod template: its serving certificate's, so a renewal rolls
/// the pod (the daemon reads its certificate at start).
pub const NOT_AFTER_ANNOTATION: &str = "balerix.ai/not-after";

/// What the controller minted or read back for one Daemon.
pub struct Material<'a> {
    pub authority: &'a Issued,
    pub serving: &'a Issued,
    pub admin_token: &'a str,
}

pub struct DaemonSecrets {
    /// `ca.crt` and `ca.key`: the operator's alone.
    pub authority: Secret,
    /// `ca.crt`: what sidecars and plugins mount to trust the Daemon.
    pub authority_config: ConfigMap,
    /// `tls.crt` and `tls.key`, mounted in the daemon pod.
    pub serving: Secret,
    /// `token`: the admin token, for the daemon and the operator.
    pub admin: Secret,
}

#[derive(Debug, Clone)]
pub struct DaemonObjects {
    pub claims: Vec<PersistentVolumeClaim>,
    pub service: Service,
    pub statefulset: StatefulSet,
    pub policy: NetworkPolicy,
    pub pool_job: Job,
}

pub struct DaemonObserved<'a> {
    pub shared_claim: Option<&'a PersistentVolumeClaim>,
    pub statefulset: Option<&'a StatefulSet>,
    pub pool: &'a JobOutcome,
}

fn name_of(daemon: &Daemon) -> Result<(&str, &str), DesiredError> {
    let name = daemon
        .metadata
        .name
        .as_deref()
        .ok_or(DesiredError::Missing("the Daemon", "metadata.name"))?;
    let namespace = daemon
        .metadata
        .namespace
        .as_deref()
        .ok_or(DesiredError::Missing("the Daemon", "metadata.namespace"))?;
    Ok((name, namespace))
}

/// §4.1: in `v1alpha1` a Daemon's version is the operator's, or absent.
pub fn version_ok(daemon: &Daemon, cfg: &OperatorConfig) -> bool {
    requested_other(daemon, cfg).is_none()
}

/// The `spec.version` a Daemon asks for when it is not the operator's.
fn requested_other<'a>(daemon: &'a Daemon, cfg: &OperatorConfig) -> Option<&'a str> {
    daemon.spec.version.as_deref().filter(|v| *v != cfg.version)
}

fn secret(
    daemon: &Daemon,
    name: String,
    type_: &str,
    data: Value,
    not_after: Option<i64>,
) -> Result<Secret, DesiredError> {
    let (daemon_name, namespace) = name_of(daemon)?;
    let mut manifest = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": labels(daemon_name, "daemon", &[]),
            "ownerReferences": [owner_of(daemon)?],
        },
        "type": type_,
        "stringData": data,
    });
    // only a certificate's Secret carries its expiry
    if let Some(t) = not_after {
        manifest["metadata"]["annotations"] = json!({ NOT_AFTER_ANNOTATION: t.to_string() });
    }
    typed(manifest)
}

pub fn daemon_secrets(
    daemon: &Daemon,
    material: &Material<'_>,
) -> Result<DaemonSecrets, DesiredError> {
    let (name, namespace) = name_of(daemon)?;
    Ok(DaemonSecrets {
        authority: secret(
            daemon,
            names::authority(name),
            "Opaque",
            json!({ "ca.crt": material.authority.cert_pem, "ca.key": material.authority.key_pem }),
            Some(material.authority.not_after),
        )?,
        authority_config: typed(json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": names::authority(name),
                "namespace": namespace,
                "labels": labels(name, "daemon", &[]),
                "ownerReferences": [owner_of(daemon)?],
            },
            "data": { "ca.crt": material.authority.cert_pem },
        }))?,
        serving: secret(
            daemon,
            names::serving(name),
            "kubernetes.io/tls",
            json!({ "tls.crt": material.serving.cert_pem, "tls.key": material.serving.key_pem }),
            Some(material.serving.not_after),
        )?,
        admin: secret(
            daemon,
            names::admin(name),
            "Opaque",
            json!({ "token": material.admin_token }),
            None,
        )?,
    })
}

pub fn daemon_objects(
    daemon: &Daemon,
    cfg: &OperatorConfig,
    serving_not_after: i64,
) -> Result<DaemonObjects, DesiredError> {
    let (name, namespace) = name_of(daemon)?;
    let owner = owner_of(daemon)?;
    let object = names::daemon(name);
    let labels = labels(name, "daemon", &[]);
    let selector =
        json!({ "balerix.ai/daemon": names::daemon_label(name), "balerix.ai/component": "daemon" });
    let metadata = json!({
        "name": object,
        "namespace": namespace,
        "labels": labels,
        "ownerReferences": [owner],
    });
    let storage = &daemon.spec.storage;
    Ok(DaemonObjects {
        claims: vec![
            claim(
                namespace,
                &names::state_claim(name),
                labels.clone(),
                &storage.state,
                "ReadWriteOnce",
            )?,
            claim(
                namespace,
                &names::shared_claim(name),
                labels.clone(),
                &storage.shared,
                "ReadWriteMany",
            )?,
        ],
        service: typed(json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": metadata,
            "spec": {
                "selector": selector,
                "ports": [{ "name": "https", "port": DAEMON_PORT, "targetPort": "https" }],
            },
        }))?,
        statefulset: typed(json!({
            "apiVersion": "apps/v1",
            "kind": "StatefulSet",
            "metadata": metadata,
            "spec": {
                "replicas": 1,
                "serviceName": object,
                "selector": { "matchLabels": selector },
                "template": {
                    "metadata": {
                        "labels": labels,
                        "annotations": { NOT_AFTER_ANNOTATION: serving_not_after.to_string() },
                    },
                    "spec": {
                        "automountServiceAccountToken": false,
                        "enableServiceLinks": false,
                        "securityContext": pod_security(),
                        "containers": [{
                            "name": "daemon",
                            "image": cfg.images.daemon,
                            "command": ["balerix"],
                            "args": [
                                "serve", "--mode", "kubernetes",
                                "--bind", format!("0.0.0.0:{DAEMON_PORT}"),
                                "--tls-cert", "/balerix/tls/tls.crt",
                                "--tls-key", "/balerix/tls/tls.key",
                                "--admin-token-file", "/balerix/admin/token",
                            ],
                            // its XDG roots: the state claim, the only writable path
                            "env": [{ "name": "HOME", "value": "/balerix/state" }],
                            "ports": [{ "name": "https", "containerPort": DAEMON_PORT }],
                            "readinessProbe": {
                                "httpGet": { "path": "/readyz", "port": "https", "scheme": "HTTPS" },
                                "periodSeconds": 5,
                            },
                            "resources": daemon.spec.resources,
                            "securityContext": container_security(),
                            "volumeMounts": [
                                { "name": "state", "mountPath": "/balerix/state" },
                                { "name": "tls", "mountPath": "/balerix/tls", "readOnly": true },
                                { "name": "admin", "mountPath": "/balerix/admin", "readOnly": true },
                            ],
                        }],
                        "volumes": [
                            { "name": "state", "persistentVolumeClaim": { "claimName": names::state_claim(name) } },
                            { "name": "tls", "secret": { "secretName": names::serving(name), "defaultMode": 0o440 } },
                            { "name": "admin", "secret": { "secretName": names::admin(name), "defaultMode": 0o440 } },
                        ],
                    },
                },
            },
        }))?,
        // §10.2: its agents and Jobs (and, later, its plugins) carry the
        // Daemon's label; the operator is named by its namespace and name.
        policy: typed(json!({
            "apiVersion": "networking.k8s.io/v1",
            "kind": "NetworkPolicy",
            "metadata": metadata,
            "spec": {
                "podSelector": { "matchLabels": selector },
                "policyTypes": ["Ingress"],
                "ingress": [{
                    "from": [
                        { "podSelector": { "matchLabels": { "balerix.ai/daemon": names::daemon_label(name) } } },
                        {
                            "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": cfg.namespace } },
                            "podSelector": { "matchLabels": { "app.kubernetes.io/name": MANAGER } },
                        },
                    ],
                    "ports": [{ "protocol": "TCP", "port": DAEMON_PORT }],
                }],
            },
        }))?,
        pool_job: daemon_pool_job(&JobContext {
            namespace,
            daemon: name,
            images: &cfg.images,
            owner: owner_of(daemon)?,
        })?,
    })
}

/// `StorageReady`, `SystemToolsReady`, `PluginsReady`, `Ready` (§4.1). A
/// `spec.resources` that is not a `ResourceRequirements` makes `Ready`
/// false, reason `InvalidResources`, before anything else.
pub fn daemon_status(
    daemon: &Daemon,
    cfg: &OperatorConfig,
    observed: &DaemonObserved<'_>,
    now: &Time,
) -> DaemonStatus {
    // The API server always sets both; were either missing, there is no
    // endpoint to report and the claim is named by a visible placeholder.
    let ident = name_of(daemon).ok();
    let shared = names::shared_claim(ident.map_or("<unnamed>", |(name, _)| name));
    let new = if let Some(other) = requested_other(daemon, cfg) {
        let message = format!(
            "spec.version {other} is not this operator's {}",
            cfg.version
        );
        vec![
            Cond::unknown("StorageReady", "VersionMismatch", ""),
            Cond::unknown("SystemToolsReady", "VersionMismatch", ""),
            Cond::unknown("PluginsReady", "VersionMismatch", ""),
            Cond::no("Ready", "VersionMismatch", &message),
        ]
    } else {
        let phase = observed
            .shared_claim
            .and_then(|c| c.status.as_ref())
            .and_then(|s| s.phase.as_deref());
        let storage = match (observed.shared_claim, phase) {
            (Some(_), Some("Bound")) => Cond::yes("StorageReady", "Bound", ""),
            (Some(_), phase) => Cond::no(
                "StorageReady",
                "ClaimPending",
                &format!(
                    "claim {shared} is {}; its class must provision ReadWriteMany",
                    phase.unwrap_or("Pending")
                ),
            ),
            (None, _) => Cond::no(
                "StorageReady",
                "ClaimPending",
                &format!("claim {shared} does not exist yet"),
            ),
        };
        let tools = match observed.pool {
            JobOutcome::Succeeded(_) => Cond::yes("SystemToolsReady", "PoolSynced", ""),
            JobOutcome::Failed(message) => Cond::no("SystemToolsReady", "PoolSyncFailed", message),
            JobOutcome::Absent | JobOutcome::Stale | JobOutcome::Running => {
                Cond::no("SystemToolsReady", "PoolSyncRunning", "")
            }
        };
        // Sub-project 4 replaces this branch with the plugin list (§20.2).
        let plugins = if daemon.spec.plugins.is_empty() {
            Cond::yes("PluginsReady", "NoPlugins", "")
        } else {
            Cond::no(
                "PluginsReady",
                "PluginsUnsupported",
                &format!(
                    "this operator runs no plugins yet; remove spec.plugins ({})",
                    daemon.spec.plugins.join(", ")
                ),
            )
        };
        // what `daemon_objects` refuses as a `Shape`, with its config path
        let resources =
            serde_json::from_value::<ResourceRequirements>(daemon.spec.resources.clone())
                .err()
                .map(|e| Cond::no("Ready", "InvalidResources", &format!("spec.resources: {e}")));
        let pod_ready = observed
            .statefulset
            .and_then(|s| s.status.as_ref())
            .and_then(|s| s.ready_replicas)
            .unwrap_or(0) // no status yet: nothing is ready
            >= 1;
        let failing = [&storage, &tools, &plugins]
            .into_iter()
            .find(|c| c.status != Some(true));
        let ready = match (resources, failing) {
            (Some(invalid), _) => invalid,
            (None, Some(failing)) => Cond::no("Ready", &failing.reason, &failing.message),
            (None, None) if pod_ready => Cond::yes("Ready", "Ready", ""),
            (None, None) => Cond::no("Ready", "DaemonNotReady", "the daemon pod is not ready"),
        };
        vec![storage, tools, plugins, ready]
    };
    let old = daemon
        .status
        .as_ref()
        .map_or(&[][..], |s| s.conditions.as_slice()); // no status yet: no old conditions
    DaemonStatus {
        observed_generation: daemon.metadata.generation,
        conditions: conditions(old, &new, daemon.metadata.generation, now),
        endpoint: ident.map(|(name, namespace)| names::endpoint(namespace, name)),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use serde_json::{Value, json};

    use super::*;
    use crate::desired::common::Images;

    fn daemon(extra: Value) -> Daemon {
        let mut spec = json!({
            "storage": {
                "state": { "storageClassName": "standard", "size": "5Gi" },
                "shared": { "storageClassName": "efs", "size": "100Gi" },
                "agent": { "storageClassName": "standard", "size": "20Gi" }
            },
            "credentials": { "claude": { "secretName": "claude-credentials" }, "github": { "secretName": "gh-token" } },
            "resources": { "requests": { "cpu": "250m", "memory": "256Mi" } }
        });
        for (k, v) in extra.as_object().unwrap() {
            spec[k] = v.clone();
        }
        serde_json::from_value(json!({
            "apiVersion": "balerix.ai/v1alpha1",
            "kind": "Daemon",
            "metadata": { "name": "default", "namespace": "team-a", "uid": "daemon-uid", "generation": 4 },
            "spec": spec
        }))
        .unwrap()
    }

    fn cfg() -> OperatorConfig {
        OperatorConfig {
            version: "0.2.0".into(),
            images: Images::for_version("0.2.0"),
            namespace: "balerix-system".into(),
        }
    }

    fn at(secs: i64) -> Time {
        Time(k8s_openapi::jiff::Timestamp::from_second(secs).unwrap())
    }

    fn status_of(
        daemon: &Daemon,
        claim_phase: Option<&str>,
        ready: i32,
        pool: JobOutcome,
    ) -> DaemonStatus {
        let claim: Option<PersistentVolumeClaim> = claim_phase.map(|p| {
            serde_json::from_value(json!({ "metadata": { "name": "c" }, "status": { "phase": p } }))
                .unwrap()
        });
        let set: StatefulSet = serde_json::from_value(json!({
            "metadata": { "name": "s" }, "status": { "replicas": 1, "readyReplicas": ready }
        }))
        .unwrap();
        daemon_status(
            daemon,
            &cfg(),
            &DaemonObserved {
                shared_claim: claim.as_ref(),
                statefulset: Some(&set),
                pool: &pool,
            },
            &at(1_800_000_000),
        )
    }

    fn cond<'a>(status: &'a DaemonStatus, type_: &str) -> (&'a str, &'a str, &'a str) {
        let c = status.conditions.iter().find(|c| c.type_ == type_).unwrap();
        (c.status.as_str(), c.reason.as_str(), c.message.as_str())
    }

    #[test]
    fn a_long_daemon_name_makes_valid_names_labels_and_selectors() {
        let long = "d".repeat(70);
        let mut d = daemon(json!({}));
        d.metadata.name = Some(long.clone());
        let o = daemon_objects(&d, &cfg(), 1_807_776_000).unwrap();
        let set = o.statefulset.metadata.name.clone().unwrap();
        assert!(set.len() <= 52, "{set}");
        let label = names::daemon_label(&long);
        assert!(label.len() <= 63);
        let labelled = [
            o.statefulset.metadata.labels.clone().unwrap(),
            o.service.metadata.labels.clone().unwrap(),
            o.policy.metadata.labels.clone().unwrap(),
            o.pool_job.metadata.labels.clone().unwrap(),
            o.claims[0].metadata.labels.clone().unwrap(),
        ];
        for labels in labelled {
            assert_eq!(labels["balerix.ai/daemon"], label);
            assert!(labels.values().all(|v| v.len() <= 63), "{labels:?}");
        }
        // the selectors find what the labels say
        let spec = o.statefulset.spec.unwrap();
        let selected = spec.selector.match_labels.unwrap();
        let template = spec.template.metadata.unwrap().labels.unwrap();
        assert!(selected.iter().all(|(k, v)| template.get(k) == Some(v)));
        assert_eq!(selected["balerix.ai/daemon"], label);
        assert_eq!(
            o.service.spec.unwrap().selector.unwrap()["balerix.ai/daemon"],
            label
        );
        let policy = serde_json::to_value(&o.policy.spec).unwrap();
        assert_eq!(
            policy["ingress"][0]["from"][0]["podSelector"]["matchLabels"]["balerix.ai/daemon"],
            json!(label)
        );
    }

    #[test]
    fn the_daemons_objects() {
        let o = daemon_objects(&daemon(json!({})), &cfg(), 1_807_776_000).unwrap();
        insta::assert_yaml_snapshot!("daemon_claims", o.claims);
        insta::assert_yaml_snapshot!("daemon_service", o.service);
        insta::assert_yaml_snapshot!("daemon_statefulset", o.statefulset);
        insta::assert_yaml_snapshot!("daemon_policy", o.policy);
        assert_eq!(
            o.pool_job.metadata.name.as_deref(),
            Some("balerix-default-pool")
        );
        // a renewed certificate rolls the pod, which reads it only at start
        let renewed = daemon_objects(&daemon(json!({})), &cfg(), 1_815_552_000).unwrap();
        assert_ne!(
            serde_json::to_value(&renewed.statefulset).unwrap()["spec"]["template"]["metadata"]["annotations"],
            serde_json::to_value(&o.statefulset).unwrap()["spec"]["template"]["metadata"]["annotations"]
        );
    }

    #[test]
    fn the_daemons_secrets_hold_the_material_and_its_expiry() {
        let authority = Issued {
            cert_pem: "CA CERT".into(),
            key_pem: "CA KEY".into(),
            not_after: 2_115_360_000,
        };
        let serving = Issued {
            cert_pem: "TLS CERT".into(),
            key_pem: "TLS KEY".into(),
            not_after: 1_807_776_000,
        };
        let s = daemon_secrets(
            &daemon(json!({})),
            &Material {
                authority: &authority,
                serving: &serving,
                admin_token: "ADMIN TOKEN",
            },
        )
        .unwrap();
        insta::assert_yaml_snapshot!("daemon_secret_authority", s.authority);
        insta::assert_yaml_snapshot!("daemon_config_authority", s.authority_config);
        insta::assert_yaml_snapshot!("daemon_secret_serving", s.serving);
        insta::assert_yaml_snapshot!("daemon_secret_admin", s.admin);
        let config = serde_json::to_string(&s.authority_config).unwrap();
        assert!(
            config.contains("CA CERT") && !config.contains("CA KEY"),
            "{config}"
        );
    }

    #[test]
    fn ready_needs_storage_the_pool_no_plugins_and_the_pod() {
        let d = daemon(json!({}));
        let s = status_of(&d, Some("Bound"), 1, JobOutcome::Succeeded("synced".into()));
        assert_eq!(cond(&s, "StorageReady"), ("True", "Bound", ""));
        assert_eq!(cond(&s, "SystemToolsReady"), ("True", "PoolSynced", ""));
        assert_eq!(cond(&s, "PluginsReady"), ("True", "NoPlugins", ""));
        assert_eq!(cond(&s, "Ready"), ("True", "Ready", ""));
        assert_eq!(
            s.endpoint.as_deref(),
            Some("https://balerix-default.team-a.svc:7643")
        );
        assert_eq!(s.observed_generation, Some(4));

        let s = status_of(&d, Some("Pending"), 1, JobOutcome::Succeeded(String::new()));
        assert_eq!(
            cond(&s, "StorageReady"),
            (
                "False",
                "ClaimPending",
                "claim balerix-default-shared is Pending; its class must provision ReadWriteMany"
            )
        );
        assert_eq!(cond(&s, "Ready").0, "False");
        assert_eq!(cond(&s, "Ready").1, "ClaimPending");
        let s = status_of(&d, None, 1, JobOutcome::Absent);
        assert_eq!(
            cond(&s, "StorageReady").2,
            "claim balerix-default-shared does not exist yet"
        );

        let s = status_of(&d, Some("Bound"), 1, JobOutcome::Running);
        assert_eq!(
            cond(&s, "SystemToolsReady"),
            ("False", "PoolSyncRunning", "")
        );
        let s = status_of(
            &d,
            Some("Bound"),
            1,
            JobOutcome::Failed("tools: system: boom".into()),
        );
        assert_eq!(
            cond(&s, "SystemToolsReady"),
            ("False", "PoolSyncFailed", "tools: system: boom")
        );
        assert_eq!(
            cond(&s, "Ready"),
            ("False", "PoolSyncFailed", "tools: system: boom")
        );

        let s = status_of(&d, Some("Bound"), 0, JobOutcome::Succeeded(String::new()));
        assert_eq!(
            cond(&s, "Ready"),
            ("False", "DaemonNotReady", "the daemon pod is not ready")
        );
    }

    /// Like a Fleet's runner (§20.5): a `spec.resources` that is not a
    /// `ResourceRequirements` is a condition with its config path, and the
    /// objects are not built from it.
    #[test]
    fn resources_that_are_not_resource_requirements_are_a_condition() {
        let d = daemon(json!({ "resources": { "requests": "lots" } }));
        assert!(matches!(
            daemon_objects(&d, &cfg(), 1_807_776_000),
            Err(DesiredError::Shape(_))
        ));
        let s = status_of(&d, Some("Bound"), 1, JobOutcome::Succeeded(String::new()));
        let (status, reason, message) = cond(&s, "Ready");
        assert_eq!((status, reason), ("False", "InvalidResources"));
        assert!(
            message.starts_with("spec.resources: ") && message.len() > "spec.resources: ".len(),
            "{message}"
        );
    }

    /// §20.2: a Daemon never reports Ready over a plugin list nothing acts on.
    #[test]
    fn a_plugin_list_is_unsupported_until_sub_project_four() {
        let d = daemon(json!({ "plugins": ["flow", "web"] }));
        let s = status_of(&d, Some("Bound"), 1, JobOutcome::Succeeded(String::new()));
        assert_eq!(
            cond(&s, "PluginsReady"),
            (
                "False",
                "PluginsUnsupported",
                "this operator runs no plugins yet; remove spec.plugins (flow, web)"
            )
        );
        assert_eq!(cond(&s, "Ready").1, "PluginsUnsupported");
    }

    #[test]
    fn another_version_is_a_mismatch_and_none_is_the_operators() {
        assert!(version_ok(&daemon(json!({})), &cfg()));
        assert!(version_ok(&daemon(json!({ "version": "0.2.0" })), &cfg()));
        let d = daemon(json!({ "version": "0.3.0" }));
        assert!(!version_ok(&d, &cfg()));
        let s = status_of(&d, Some("Bound"), 1, JobOutcome::Succeeded(String::new()));
        assert_eq!(
            cond(&s, "Ready"),
            (
                "False",
                "VersionMismatch",
                "spec.version 0.3.0 is not this operator's 0.2.0"
            )
        );
        assert_eq!(cond(&s, "StorageReady").0, "Unknown");
    }
}
